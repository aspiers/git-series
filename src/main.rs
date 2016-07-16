extern crate chrono;
#[macro_use]
extern crate clap;
extern crate git2;
extern crate isatty;
#[macro_use]
extern crate quick_error;
extern crate tempdir;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::Read;
use std::io::Write as IoWrite;
use std::process::Command;
use chrono::offset::TimeZone;
use clap::{App, AppSettings, Arg, ArgGroup, ArgMatches, SubCommand};
use git2::{Commit, Diff, ObjectType, Oid, Reference, Repository, TreeBuilder};
use tempdir::TempDir;

quick_error! {
    #[derive(Debug)]
    enum Error {
        CommitNoChanges {}
        CheckoutConflict {}
        Git2(err: git2::Error) {
            from()
            cause(err)
            display("{}", err)
        }
        IO(err: std::io::Error) {
            from()
            cause(err)
            display("{}", err)
        }
        Msg(msg: String) {
            from()
            from(s: &'static str) -> (s.to_string())
            description(msg)
            display("{}", msg)
        }
        Utf8Error(err: std::str::Utf8Error) {
            from()
            cause(err)
            display("{}", err)
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

const COMMIT_MESSAGE_COMMENT: &'static str = "
# Please enter the commit message for your changes. Lines starting
# with '#' will be ignored, and an empty message aborts the commit.
";
const COVER_LETTER_COMMENT: &'static str = "
# Please enter the cover letter for your changes. Lines starting
# with '#' will be ignored, and an empty message aborts the change.
";
const REBASE_COMMENT: &'static str = "\
#
# Commands:
# p, pick = use commit
# r, reword = use commit, but edit the commit message
# e, edit = use commit, but stop for amending
# s, squash = use commit, but meld into previous commit
# f, fixup = like \"squash\", but discard this commit's log message
# x, exec = run command (the rest of the line) using shell
# d, drop = remove commit
#
# These lines can be re-ordered; they are executed from top to bottom.
#
# If you remove a line here THAT COMMIT WILL BE LOST.
#
# However, if you remove everything, the rebase will be aborted.
";
const SCISSOR_LINE: &'static str = "\
# ------------------------ >8 ------------------------";
const SCISSOR_COMMENT: &'static str = "\
# Do not touch the line above.
# Everything below will be removed.
";

const SHELL_METACHARS: &'static str = "|&;<>()$`\\\"' \t\n*?[#~=%";

const SERIES_PREFIX: &'static str = "refs/heads/git-series/";
const SHEAD_REF: &'static str = "refs/SHEAD";
const STAGED_PREFIX: &'static str = "refs/git-series-internals/staged/";
const WORKING_PREFIX: &'static str = "refs/git-series-internals/working/";

const GIT_FILEMODE_BLOB: u32 = 0o100644;
const GIT_FILEMODE_COMMIT: u32 = 0o160000;

fn zero_oid() -> Oid {
    Oid::from_bytes(b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00").unwrap()
}

fn peel_to_commit(r: Reference) -> Result<Commit> {
    Ok(try!(try!(r.peel(ObjectType::Commit)).into_commit().map_err(|obj| format!("Internal error: expected a commit: {}", obj.id()))))
}

fn commit_obj_summarize_components(commit: &mut Commit) -> Result<(String, String)> {
    let short_id_buf = try!(commit.as_object().short_id());
    let short_id = short_id_buf.as_str().unwrap();
    let summary = String::from_utf8_lossy(commit.summary_bytes().unwrap());
    Ok((short_id.to_string(), summary.to_string()))
}

fn commit_summarize_components(repo: &Repository, id: Oid) -> Result<(String, String)> {
    let mut commit = try!(repo.find_commit(id));
    commit_obj_summarize_components(&mut commit)
}

fn commit_obj_summarize(commit: &mut Commit) -> Result<String> {
    let (short_id, summary) = try!(commit_obj_summarize_components(commit));
    Ok(format!("{} {}", short_id, summary))
}

fn commit_summarize(repo: &Repository, id: Oid) -> Result<String> {
    let mut commit = try!(repo.find_commit(id));
    commit_obj_summarize(&mut commit)
}

fn notfound_to_none<T>(result: std::result::Result<T, git2::Error>) -> Result<Option<T>> {
    match result {
        Err(ref e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(x) => Ok(Some(x)),
    }
}

// If current_id_opt is Some, acts like reference_matching.  If current_id_opt is None, acts like
// reference.
fn reference_matching_opt<'repo>(repo: &'repo Repository, name: &str, id: Oid, force: bool, current_id_opt: Option<Oid>, log_message: &str) -> Result<Reference<'repo>> {
    match current_id_opt {
        None => Ok(try!(repo.reference(name, id, force, log_message))),
        Some(current_id) => Ok(try!(repo.reference_matching(name, id, force, current_id, log_message))),
    }
}

fn parents_from_ids(repo: &Repository, mut parents: Vec<Oid>) -> Result<Vec<Commit>> {
    parents.sort();
    parents.dedup();
    parents.drain(..).map(|id| Ok(try!(repo.find_commit(id)))).collect::<Result<Vec<Commit>>>()
}

struct Internals<'repo> {
    staged: TreeBuilder<'repo>,
    working: TreeBuilder<'repo>,
}

impl<'repo> Internals<'repo> {
    fn read(repo: &'repo Repository) -> Result<Self> {
        let shead = try!(repo.find_reference(SHEAD_REF));
        let series_name = try!(shead_series_name(&shead));
        let mut internals = try!(Internals::read_series(repo, &series_name));
        try!(internals.update_series(repo));
        Ok(internals)
    }

    fn read_series(repo: &'repo Repository, series_name: &str) -> Result<Self> {
        let maybe_get_ref = |prefix: &str| -> Result<TreeBuilder<'repo>> {
            match try!(notfound_to_none(repo.refname_to_id(&format!("{}{}", prefix, series_name)))) {
                Some(id) => {
                    let c = try!(repo.find_commit(id));
                    let t = try!(c.tree());
                    Ok(try!(repo.treebuilder(Some(&t))))
                }
                None => Ok(try!(repo.treebuilder(None))),
            }
        };
        Ok(Internals {
            staged: try!(maybe_get_ref(STAGED_PREFIX)),
            working: try!(maybe_get_ref(WORKING_PREFIX)),
        })
    }

    fn exists(repo: &'repo Repository, series_name: &str) -> Result<bool> {
        for prefix in [STAGED_PREFIX, WORKING_PREFIX].iter() {
            let prefixed_name = format!("{}{}", prefix, series_name);
            if try!(notfound_to_none(repo.refname_to_id(&prefixed_name))).is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // Returns true if it had anything to delete.
    fn delete(repo: &'repo Repository, series_name: &str) -> Result<bool> {
        let mut deleted_any = false;
        for prefix in [STAGED_PREFIX, WORKING_PREFIX].iter() {
            let prefixed_name = format!("{}{}", prefix, series_name);
            if let Some(mut r) = try!(notfound_to_none(repo.find_reference(&prefixed_name))) {
                try!(r.delete());
                deleted_any = true;
            }
        }
        Ok(deleted_any)
    }

    fn update_series(&mut self, repo: &'repo Repository) -> Result<()> {
        let head_id = try!(repo.refname_to_id("HEAD"));
        try!(self.working.insert("series", head_id, GIT_FILEMODE_COMMIT as i32));
        Ok(())
    }

    fn write(&self, repo: &'repo Repository) -> Result<()> {
        let config = try!(repo.config());
        let author = try!(get_signature(&config, "AUTHOR"));
        let committer = try!(get_signature(&config, "COMMITTER"));

        let shead = try!(repo.find_reference(SHEAD_REF));
        let series_name = try!(shead_series_name(&shead));
        let maybe_commit = |prefix: &str, tb: &TreeBuilder| -> Result<()> {
            let tree_id = try!(tb.write());
            let refname = format!("{}{}", prefix, series_name);
            let old_commit_id = try!(notfound_to_none(repo.refname_to_id(&refname)));
            if let Some(id) = old_commit_id {
                let c = try!(repo.find_commit(id));
                if c.tree_id() == tree_id {
                    return Ok(());
                }
            }
            let tree = try!(repo.find_tree(tree_id));
            let mut parents = Vec::new();
            // Include all commits from tree, to keep them reachable and fetchable. Include base,
            // because series might not have it as an ancestor; we don't enforce that until commit.
            for e in tree.iter() {
                if e.kind() == Some(git2::ObjectType::Commit) {
                    parents.push(e.id());
                }
            }
            let parents = try!(parents_from_ids(repo, parents));
            let parents_ref: Vec<&_> = parents.iter().collect();
            let commit_id = try!(repo.commit(None, &author, &committer, &refname, &tree, &parents_ref));
            try!(repo.reference_ensure_log(&refname));
            try!(reference_matching_opt(repo, &refname, commit_id, true, old_commit_id, &format!("commit: {}", refname)));
            Ok(())
        };
        try!(maybe_commit(STAGED_PREFIX, &self.staged));
        try!(maybe_commit(WORKING_PREFIX, &self.working));
        Ok(())
    }
}

fn diff_empty(diff: &Diff) -> bool {
    diff.deltas().len() == 0
}

fn add(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let mut internals = try!(Internals::read(repo));
    for file in m.values_of_os("change").unwrap() {
        match try!(internals.working.get(file)) {
            Some(entry) => { try!(internals.staged.insert(file, entry.id(), entry.filemode())); }
            None => {
                if try!(internals.staged.get(file)).is_some() {
                    try!(internals.staged.remove(file));
                }
            }
        }
    }
    internals.write(repo)
}

fn unadd(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let shead = try!(repo.find_reference(SHEAD_REF));
    let started = {
        let shead_target = try!(shead.symbolic_target().ok_or("SHEAD not a symbolic reference"));
        try!(notfound_to_none(repo.find_reference(shead_target))).is_some()
    };

    let mut internals = try!(Internals::read(repo));
    if started {
        let shead_commit = try!(peel_to_commit(shead));
        let shead_tree = try!(shead_commit.tree());

        for file in m.values_of("change").unwrap() {
            match shead_tree.get_name(file) {
                Some(entry) => {
                    try!(internals.staged.insert(file, entry.id(), entry.filemode()));
                }
                None => { try!(internals.staged.remove(file)); }
            }
        }
    } else {
        for file in m.values_of("change").unwrap() {
            try!(internals.staged.remove(file))
        }
    }
    internals.write(repo)
}

fn shead_series_name(shead: &Reference) -> Result<String> {
    let shead_target = try!(shead.symbolic_target().ok_or("SHEAD not a symbolic reference"));
    if !shead_target.starts_with(SERIES_PREFIX) {
        return Err(format!("SHEAD does not start with {}", SERIES_PREFIX).into());
    }
    Ok(shead_target[SERIES_PREFIX.len()..].to_string())
}

fn series(repo: &Repository) -> Result<()> {
    let mut refs = Vec::new();
    for prefix in [SERIES_PREFIX, STAGED_PREFIX, WORKING_PREFIX].iter() {
        let l = prefix.len();
        for r in try!(repo.references_glob(&[prefix, "*"].concat())).names() {
            refs.push(try!(r)[l..].to_string());
        }
    }
    let shead_target = if let Some(shead) = try!(notfound_to_none(repo.find_reference(SHEAD_REF))) {
        Some(try!(shead_series_name(&shead)).to_string())
    } else {
        None
    };
    refs.extend(shead_target.clone().into_iter());
    refs.sort();
    refs.dedup();
    for name in refs.iter() {
        let star = if Some(name) == shead_target.as_ref() { '*' } else { ' ' };
        let new = if try!(notfound_to_none(repo.refname_to_id(&format!("{}{}", SERIES_PREFIX, name)))).is_none() {
            " (new, no commits yet)"
        } else {
            ""
        };
        println!("{} {}{}", star, name, new);
    }
    if refs.is_empty() {
        println!("No series; use \"git series start <name>\" to start");
    }
    Ok(())
}

fn start(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let name = m.value_of("name").unwrap();
    let prefixed_name = &[SERIES_PREFIX, name].concat();
    let branch_exists = try!(notfound_to_none(repo.refname_to_id(&prefixed_name))).is_some()
                        || try!(Internals::exists(repo, name));
    if branch_exists {
        return Err(format!("Series {} already exists.\nUse checkout to resume working on an existing patch series.", name).into());
    }
    try!(repo.reference_symbolic(SHEAD_REF, &prefixed_name, true, &format!("git series start {}", name)));

    let internals = try!(Internals::read(repo));
    try!(internals.write(repo));
    Ok(())
}

fn checkout_tree(repo: &Repository, treeish: &git2::Object) -> Result<()> {
    let mut conflicts = Vec::new();
    let mut dirty = Vec::new();
    let result = {
        let mut opts = git2::build::CheckoutBuilder::new();
        opts.safe();
        opts.notify_on(git2::CHECKOUT_NOTIFICATION_CONFLICT | git2::CHECKOUT_NOTIFICATION_DIRTY);
        opts.notify(|t, path, _, _, _| {
            let path = path.unwrap().to_owned();
            if t == git2::CHECKOUT_NOTIFICATION_CONFLICT {
                conflicts.push(path);
            } else if t == git2::CHECKOUT_NOTIFICATION_DIRTY {
                dirty.push(path);
            }
            true
        });
        if isatty::stdout_isatty() {
            opts.progress(|_, completed, total| {
                let total = total.to_string();
                print!("\rChecking out files: {1:0$}/{2}", total.len(), completed, total);
            });
        }
        repo.checkout_tree(treeish, Some(&mut opts))
    };
    match result {
        Err(ref e) if e.code() == git2::ErrorCode::Conflict => {
            let mut stderr = std::io::stderr();
            writeln!(stderr, "error: Your changes to the following files would be overwritten by checkout:").unwrap();
            for path in conflicts {
                writeln!(stderr, "        {}", path.to_string_lossy()).unwrap();
            }
            writeln!(stderr, "Please, commit your changes or stash them before you switch series.").unwrap();
            return Err(Error::CheckoutConflict);
        }
        _ => try!(result),
    }
    println!("");
    if !dirty.is_empty() {
        let mut stderr = std::io::stderr();
        writeln!(stderr, "Files with changes unaffected by checkout:").unwrap();
        for path in dirty {
            writeln!(stderr, "        {}", path.to_string_lossy()).unwrap();
        }
    }
    Ok(())
}

fn checkout(repo: &Repository, m: &ArgMatches) -> Result<()> {
    match repo.state() {
        git2::RepositoryState::Clean => (),
        s => { return Err(format!("{:?} in progress; cannot checkout patch series", s).into()); }
    }
    let name = m.value_of("name").unwrap();
    let prefixed_name = &[SERIES_PREFIX, name].concat();
    // Make sure the ref exists
    let branch_exists = try!(notfound_to_none(repo.refname_to_id(&prefixed_name))).is_some()
                        || try!(Internals::exists(repo, name));
    if !branch_exists {
        return Err(format!("Series {} does not exist.\nUse \"git series start <name>\" to start a new patch series.", name).into());
    }

    let internals = try!(Internals::read_series(repo, name));
    let new_head_id = try!(try!(internals.working.get("series")).ok_or(format!("Could not find \"series\" in working version of \"{}\"", name))).id();
    let new_head = try!(repo.find_commit(new_head_id)).into_object();

    try!(checkout_tree(repo, &new_head));

    let head = try!(repo.head());
    let head_commit = try!(peel_to_commit(head));
    let head_id = head_commit.as_object().id();
    println!("Previous HEAD position was {}", try!(commit_summarize(&repo, head_id)));

    try!(repo.reference_symbolic(SHEAD_REF, &prefixed_name, true, &format!("git series checkout {}", name)));

    // git status parses this reflog string; the prefix must remain "checkout: moving from ".
    try!(repo.reference("HEAD", new_head_id, true, &format!("checkout: moving from {} to {} (git series checkout {})", head_id, new_head_id, name)));
    println!("HEAD is now detached at {}", try!(commit_summarize(&repo, new_head_id)));

    Ok(())
}

fn base(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let mut internals = try!(Internals::read(repo));

    let current_base_id = match try!(internals.working.get("base")) {
        Some(entry) => entry.id(),
        _ => zero_oid(),
    };

    if !m.is_present("delete") && !m.is_present("base") {
        if current_base_id.is_zero() {
            return Err("Patch series has no base set".into());
        } else {
            println!("{}", current_base_id);
            return Ok(());
        }
    }

    let new_base_id = if m.is_present("delete") {
        zero_oid()
    } else {
        let base = m.value_of("base").unwrap();
        let base_object = try!(repo.revparse_single(base));
        let base_id = base_object.id();
        let s_working_series = try!(try!(internals.working.get("series")).ok_or("Could not find entry \"series\" in working vesion of current series"));
        if base_id != s_working_series.id() && !try!(repo.graph_descendant_of(s_working_series.id(), base_id)) {
            return Err(format!("Cannot set base to {}: not an ancestor of the patch series {}", base, s_working_series.id()).into());
        }
        base_id
    };

    if current_base_id == new_base_id {
        return Err("Base unchanged".into());
    }

    if !current_base_id.is_zero() {
        println!("Previous base was {}", try!(commit_summarize(&repo, current_base_id)));
    }

    if new_base_id.is_zero() {
        try!(internals.working.remove("base"));
        try!(internals.write(repo));
        println!("Cleared patch series base");
    } else {
        try!(internals.working.insert("base", new_base_id, GIT_FILEMODE_COMMIT as i32));
        try!(internals.write(repo));
        println!("Set patch series base to {}", try!(commit_summarize(&repo, new_base_id)));
    }

    Ok(())
}

fn detach(repo: &Repository) -> Result<()> {
    match repo.find_reference(SHEAD_REF) {
        Ok(mut r) => try!(r.delete()),
        Err(_) => { return Err("No current patch series to detach from.".into()); }
    }
    Ok(())
}

fn delete(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let name = m.value_of("name").unwrap();
    if let Ok(shead) = repo.find_reference(SHEAD_REF) {
        let shead_target = try!(shead_series_name(&shead));
        if shead_target == name {
            return Err(format!("Cannot delete the current series \"{}\"; detach first.", name).into());
        }
    }
    let prefixed_name = &[SERIES_PREFIX, name].concat();
    let deleted_ref = if let Some(mut r) = try!(notfound_to_none(repo.find_reference(prefixed_name))) {
        try!(r.delete());
        true
    } else {
        false
    };
    let deleted_internals = try!(Internals::delete(repo, name));
    if !deleted_ref && !deleted_internals {
        return Err(format!("Nothing to delete: series \"{}\" does not exist.", name).into());
    }
    Ok(())
}

fn get_editor(config: &git2::Config) -> Result<OsString> {
    if let Some(e) = env::var_os("GIT_EDITOR") {
        return Ok(e);
    }
    if let Ok(e) = config.get_path("core.editor") {
        return Ok(e.into());
    }
    let terminal_is_dumb = match env::var_os("TERM") {
        None => true,
        Some(t) => t.as_os_str() == "dumb",
    };
    if !terminal_is_dumb {
        if let Some(e) = env::var_os("VISUAL") {
            return Ok(e);
        }
    }
    if let Some(e) = env::var_os("EDITOR") {
        return Ok(e);
    }
    if terminal_is_dumb {
        return Err("TERM unset or \"dumb\" but EDITOR unset".into());
    }
    return Ok("vi".into());
}

// Get the pager to use; with for_cmd set, get the pager for use by the
// specified git command.  If get_pager returns None, don't use a pager.
fn get_pager(config: &git2::Config, for_cmd: Option<&str>) -> Option<OsString> {
    if !isatty::stdout_isatty() {
        return None;
    }
    // pager.cmd can contain a boolean (if false, force no pager) or a
    // command-specific pager; only treat it as a command if it doesn't parse
    // as a boolean.
    let maybe_pager = for_cmd.and_then(|cmd| config.get_path(&format!("pager.{}", cmd)).ok());
    let (cmd_want_pager, cmd_pager) = maybe_pager.map_or((true, None), |p|
            if let Ok(b) = git2::Config::parse_bool(&p) {
                (b, None)
            } else {
                (true, Some(p))
            }
        );
    if !cmd_want_pager {
        return None;
    }
    let pager =
        if let Some(e) = env::var_os("GIT_PAGER") {
            Some(e)
        } else if let Some(p) = cmd_pager {
            Some(p.into())
        } else if let Ok(e) = config.get_path("core.pager") {
            Some(e.into())
        } else if let Some(e) = env::var_os("PAGER") {
            Some(e)
        } else {
            Some("less".into())
        };
    pager.and_then(|p| if p.is_empty() || p == OsString::from("cat") { None } else { Some(p) })
}

/// Construct a Command, using the shell if the command contains shell metachars
fn cmd_maybe_shell<S: AsRef<OsStr>>(program: S, args: bool) -> Command {
    if program.as_ref().to_string_lossy().contains(|c| SHELL_METACHARS.contains(c)) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c");
        if args {
            let mut program_with_args = program.as_ref().to_os_string();
            program_with_args.push(" \"$@\"");
            cmd.arg(program_with_args).arg(program);
        } else {
            cmd.arg(program);
        }
        cmd
    } else {
        Command::new(program)
    }
}

fn run_editor<S: AsRef<OsStr>>(config: &git2::Config, filename: S) -> Result<()> {
    let editor = try!(get_editor(&config));
    let editor_status = try!(cmd_maybe_shell(editor, true).arg(&filename).status());
    if !editor_status.success() {
        return Err(format!("Editor exited with status {}", editor_status).into());
    }
    Ok(())
}

enum Pager {
    Pager(std::process::Child),
    NoPager(std::io::Stdout),
}

impl Pager {
    fn auto(config: &git2::Config, for_cmd: Option<&str>) -> Result<Pager> {
        if let Some(pager) = get_pager(config, for_cmd) {
            let mut cmd = cmd_maybe_shell(pager, false);
            cmd.stdin(std::process::Stdio::piped());
            if env::var_os("LESS").is_none() {
                cmd.env("LESS", "FRX");
            }
            if env::var_os("LV").is_none() {
                cmd.env("LV", "-c");
            }
            let child = try!(cmd.spawn());
            Ok(Pager::Pager(child))
        } else {
            Ok(Pager::NoPager(std::io::stdout()))
        }
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        if let Pager::Pager(ref mut child) = *self {
            let status = child.wait().unwrap();
            if !status.success() {
                writeln!(std::io::stderr(), "Pager exited with status {}", status).unwrap();
            }
        }
    }
}

impl IoWrite for Pager {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match *self {
            Pager::Pager(ref mut child) => child.stdin.as_mut().unwrap().write(buf),
            Pager::NoPager(ref mut stdout) => stdout.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match *self {
            Pager::Pager(ref mut child) => child.stdin.as_mut().unwrap().flush(),
            Pager::NoPager(ref mut stdout) => stdout.flush(),
        }
    }
}

fn get_signature(config: &git2::Config, which: &str) -> Result<git2::Signature<'static>> {
    let name_var = ["GIT_", which, "_NAME"].concat();
    let email_var = ["GIT_", which, "_EMAIL"].concat();
    let which_lc = which.to_lowercase();
    let name = try!(env::var(&name_var).or_else(
            |_| config.get_string("user.name").or_else(
                |_| Err(format!("Could not determine {} name: checked ${} and user.name in git config", which_lc, name_var)))));
    let email = try!(env::var(&email_var).or_else(
            |_| config.get_string("user.email").or_else(
                |_| env::var("EMAIL").or_else(
                    |_| Err(format!("Could not determine {} email: checked ${}, user.email in git config, and $EMAIL", which_lc, email_var))))));
    Ok(try!(git2::Signature::now(&name, &email)))
}

fn write_status(status: &mut String, diff: &Diff, heading: &str, show_hints: bool, hints: &[&str]) -> Result<bool> {
    let mut changes = false;

    try!(diff.foreach(&mut |delta, _| {
        if !changes {
            changes = true;
            writeln!(status, "{}", heading).unwrap();
            if show_hints {
                for hint in hints {
                    writeln!(status, "  ({})", hint).unwrap();
                }
            }
            writeln!(status, "").unwrap();
        }
        writeln!(status, "        {:?}:   {}", delta.status(), delta.old_file().path().unwrap().to_str().unwrap()).unwrap();
        true
    }, None, None, None));

    if changes {
        writeln!(status, "").unwrap();
    }

    Ok(changes)
}

fn commit_status(repo: &Repository, m: &ArgMatches, do_status: bool) -> Result<()> {
    let config = try!(repo.config());
    let shead = match repo.find_reference(SHEAD_REF) {
        Err(ref e) if e.code() == git2::ErrorCode::NotFound => { println!("No series; use \"git series start <name>\" to start"); return Ok(()); }
        result => try!(result),
    };
    let series_name = try!(shead_series_name(&shead));
    let mut status = String::new();
    writeln!(status, "On series {}", series_name).unwrap();

    let mut internals = try!(Internals::read(repo));
    let working_tree = try!(repo.find_tree(try!(internals.working.write())));
    let staged_tree = try!(repo.find_tree(try!(internals.staged.write())));

    let shead_commit = match shead.resolve() {
        Ok(r) => Some(try!(peel_to_commit(r))),
        Err(ref e) if e.code() == git2::ErrorCode::NotFound => {
            writeln!(status, "\nInitial series commit\n").unwrap();
            None
        }
        Err(e) => try!(Err(e)),
    };
    let shead_tree = match shead_commit {
        Some(ref c) => Some(try!(c.tree())),
        None => None,
    };

    let commit_all = m.is_present("all");

    let (changes, tree, diff) = if commit_all {
        let diff = try!(repo.diff_tree_to_tree(shead_tree.as_ref(), Some(&working_tree), None));
        let changes = try!(write_status(&mut status, &diff, "Changes to be committed:", false, &[]));
        if !changes {
            writeln!(status, "nothing to commit; series unchanged").unwrap();
        }
        (changes, working_tree, diff)
    } else {
        let diff = try!(repo.diff_tree_to_tree(shead_tree.as_ref(), Some(&staged_tree), None));
        let changes_to_be_committed = try!(write_status(&mut status, &diff,
                "Changes to be committed:", do_status,
                &["use \"git series commit\" to commit",
                  "use \"git series unadd <file>...\" to undo add"]));

        let diff_not_staged = try!(repo.diff_tree_to_tree(Some(&staged_tree), Some(&working_tree), None));
        let changes_not_staged = try!(write_status(&mut status, &diff_not_staged,
                "Changes not staged for commit:", do_status,
                &["use \"git series add <file>...\" to update what will be committed"]));

        if !changes_to_be_committed {
            if changes_not_staged {
                writeln!(status, "no changes added to commit (use \"git series add\" or \"git series commit -a\")").unwrap();
            } else {
                writeln!(status, "nothing to commit; series unchanged").unwrap();
            }
        }

        (changes_to_be_committed, staged_tree, diff)
    };

    if do_status || !changes {
        print!("{}", status);
        if !do_status {
            return Err(Error::CommitNoChanges);
        }
        return Ok(());
    }

    // Check that the commit includes the series
    let series_id = match tree.get_name("series") {
        None => { return Err(concat!("Cannot commit: initial commit must include \"series\"\n",
                                     "Use \"git series add series\" or \"git series commit -a\"").into()); }
        Some(series) => series.id()
    };

    // Check that the base is still an ancestor of the series
    if let Some(base) = tree.get_name("base") {
        if base.id() != series_id && !try!(repo.graph_descendant_of(series_id, base.id())) {
            let (base_short_id, base_summary) = try!(commit_summarize_components(&repo, base.id()));
            let (series_short_id, series_summary) = try!(commit_summarize_components(&repo, series_id));
            return Err(format!(concat!(
                       "Cannot commit: base {} is not an ancestor of patch series {}\n",
                       "base   {} {}\n",
                       "series {} {}"),
                       base_short_id, series_short_id,
                       base_short_id, base_summary,
                       series_short_id, series_summary).into());
        }
    }

    let msg = match m.value_of("m") {
        Some(s) => s.to_string(),
        None => {
            let filename = repo.path().join("SCOMMIT_EDITMSG");
            let mut file = try!(File::create(&filename));
            try!(write!(file, "{}", COMMIT_MESSAGE_COMMENT));
            for line in status.lines() {
                if line.is_empty() {
                    try!(writeln!(file, "#"));
                } else {
                    try!(writeln!(file, "# {}", line));
                }
            }
            if m.is_present("verbose") {
                try!(writeln!(file, "{}\n{}", SCISSOR_LINE, SCISSOR_COMMENT));
                try!(write_diff(&mut file, &diff));
            }
            drop(file);
            try!(run_editor(&config, &filename));
            let mut file = try!(File::open(&filename));
            let mut msg = String::new();
            try!(file.read_to_string(&mut msg));
            if let Some(scissor_index) = msg.find(SCISSOR_LINE) {
                msg.truncate(scissor_index);
            }
            try!(git2::message_prettify(msg, git2::DEFAULT_COMMENT_CHAR))
        }
    };
    if msg.is_empty() {
        return Err("Aborting series commit due to empty commit message.".into());
    }

    let author = try!(get_signature(&config, "AUTHOR"));
    let committer = try!(get_signature(&config, "COMMITTER"));
    let mut parents: Vec<Oid> = Vec::new();
    // Include all commits from tree, to keep them reachable and fetchable.
    for e in tree.iter() {
        if e.kind() == Some(git2::ObjectType::Commit) && e.name().unwrap() != "base" {
            parents.push(e.id())
        }
    }
    let parents = try!(parents_from_ids(repo, parents));
    let parents_ref: Vec<&_> = shead_commit.iter().chain(parents.iter()).collect();
    let new_commit_oid = try!(repo.commit(Some(SHEAD_REF), &author, &committer, &msg, &tree, &parents_ref));

    if commit_all {
        internals.staged = try!(repo.treebuilder(Some(&tree)));
        try!(internals.write(repo));
    }

    let (new_commit_short_id, new_commit_summary) = try!(commit_summarize_components(&repo, new_commit_oid));
    println!("[{} {}] {}", series_name, new_commit_short_id, new_commit_summary);

    Ok(())
}

fn cover(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let mut internals = try!(Internals::read(repo));

    let (working_cover_id, working_cover_content) = match try!(internals.working.get("cover")) {
        None => (zero_oid(), String::new()),
        Some(entry) => (entry.id(), try!(std::str::from_utf8(try!(repo.find_blob(entry.id())).content())).to_string()),
    };

    if m.is_present("delete") {
        if working_cover_id.is_zero() {
            return Err("No cover to delete".into());
        }
        try!(internals.working.remove("cover"));
        try!(internals.write(repo));
        println!("Deleted cover letter");
        return Ok(());
    }

    let filename = repo.path().join("COVER_EDITMSG");
    let mut file = try!(File::create(&filename));
    if working_cover_content.is_empty() {
        try!(write!(file, "{}", COVER_LETTER_COMMENT));
    } else {
        try!(write!(file, "{}", working_cover_content));
    }
    drop(file);
    let config = try!(repo.config());
    try!(run_editor(&config, &filename));
    let mut file = try!(File::open(&filename));
    let mut msg = String::new();
    try!(file.read_to_string(&mut msg));
    let msg = try!(git2::message_prettify(msg, git2::DEFAULT_COMMENT_CHAR));
    if msg.is_empty() {
        return Err("Empty cover letter; not changing.\n(To delete the cover letter, use \"git series -d\".)".into());
    }

    let new_cover_id = try!(repo.blob(msg.as_bytes()));
    if new_cover_id == working_cover_id {
        println!("Cover letter unchanged");
    } else {
        try!(internals.working.insert("cover", new_cover_id, GIT_FILEMODE_BLOB as i32));
        try!(internals.write(repo));
        println!("Updated cover letter");
    }

    Ok(())
}

fn date_822(t: git2::Time) -> String {
    let offset = chrono::offset::fixed::FixedOffset::east(t.offset_minutes()*60);
    let datetime = offset.timestamp(t.seconds(), 0);
    datetime.to_rfc2822()
}

fn shortlog(commits: &mut [Commit]) -> String {
    let mut s = String::new();
    let mut author_map = std::collections::HashMap::new();

    for mut commit in commits {
        let author = commit.author().name().unwrap().to_string();
        author_map.entry(author).or_insert(Vec::new()).push(commit.summary().unwrap().to_string());
    }

    let mut authors: Vec<_> = author_map.keys().collect();
    authors.sort();
    let mut first = true;
    for author in authors {
        if first {
            first = false;
        } else {
            writeln!(s, "").unwrap();
        }
        let summaries = author_map.get(author).unwrap();
        writeln!(s, "{} ({}):", author, summaries.len()).unwrap();
        for summary in summaries {
            writeln!(s, "  {}", summary).unwrap();
        }
    }

    s
}

fn ascii_isalnum(c: char) -> bool {
    (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
}

fn sanitize_summary(summary: &str) -> String {
    let mut s = String::with_capacity(summary.len());
    let mut prev_dot = false;
    let mut need_space = false;
    for c in summary.chars() {
        if ascii_isalnum(c) || c == '_' || c == '.' {
            if need_space {
                s.push('-');
                need_space = false;
            }
            if !(prev_dot && c == '.') {
                s.push(c);
            }
        } else {
            if !s.is_empty() {
                need_space = true;
            }
        }
        prev_dot = c == '.';
    }
    let end = s.trim_right_matches(|c| c == '.' || c == '-').len();
    s.truncate(end);
    s
}

#[test]
fn test_sanitize_summary() {
    let tests = vec![
        ("", ""),
        ("!!!!!", ""),
        ("Test", "Test"),
        ("Test case", "Test-case"),
        ("Test    case", "Test-case"),
        ("    Test    case    ", "Test-case"),
        ("...Test...case...", ".Test.case"),
        ("...Test...case.!!", ".Test.case"),
        (".!.Test.!.case.!.", ".-.Test.-.case"),
    ];
    for (summary, sanitized) in tests {
        assert_eq!(sanitize_summary(summary), sanitized.to_string());
    }
}

fn split_message(message: &str) -> (&str, &str) {
    let mut iter = message.splitn(2, '\n');
    let subject = iter.next().unwrap().trim_right();
    let body = iter.next().map(|s| s.trim_left()).unwrap_or("");
    (subject, body)
}

fn diffstat(diff: &Diff) -> Result<String> {
    let stats = try!(diff.stats());
    let stats_buf = try!(stats.to_buf(git2::DIFF_STATS_FULL|git2::DIFF_STATS_INCLUDE_SUMMARY, 72));
    Ok(stats_buf.as_str().unwrap().to_string())
}

fn write_diff<W: IoWrite>(f: &mut W, diff: &Diff) -> Result<()> {
    Ok(try!(diff.print(git2::DiffFormat::Patch, |_, _, l| {
        let o = l.origin();
        if o == '+' || o == '-' || o == ' ' {
            f.write_all(&[o as u8]).unwrap();
        }
        f.write_all(l.content()).unwrap();
        true
    })))
}

fn mail_signature() -> String {
    format!("-- \ngit-series {}", crate_version!())
}

fn format(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let config = try!(repo.config());
    let to_stdout = m.is_present("stdout");

    let shead_commit = try!(peel_to_commit(try!(try!(repo.find_reference(SHEAD_REF)).resolve())));
    let stree = try!(shead_commit.tree());

    let series = try!(stree.get_name("series").ok_or("Internal error: series did not contain \"series\""));
    let base = try!(stree.get_name("base").ok_or("Cannot format series; no base set.\nUse \"git series base\" to set base."));

    let mut revwalk = try!(repo.revwalk());
    revwalk.set_sorting(git2::SORT_TOPOLOGICAL|git2::SORT_REVERSE);
    try!(revwalk.push(series.id()));
    try!(revwalk.hide(base.id()));
    let mut commits: Vec<Commit> = try!(revwalk.map(|c| {
        let id = try!(c);
        let commit = try!(repo.find_commit(id));
        if commit.parent_ids().count() > 1 {
            return Err(format!("Error: cannot format merge commit as patch:\n{}", try!(commit_summarize(repo, id))).into());
        }
        Ok(commit)
    }).collect::<Result<_>>());
    if commits.is_empty() {
        return Err("No patches to format; series and base identical.".into());
    }

    let author = try!(get_signature(&config, "AUTHOR"));
    let author_name = author.name().unwrap();
    let author_email = author.email().unwrap();
    let message_id_suffix = format!("{}.git-series.{}", author.when().seconds(), author_email);

    let cover_entry = stree.get_name("cover");
    let root_message_id = if cover_entry.is_some() {
        format!("<cover.{}.{}>", shead_commit.id(), message_id_suffix)
    } else {
        format!("<{}.{}>", commits.first().unwrap().id(), message_id_suffix)
    };

    let signature = mail_signature();

    let mut out : Box<IoWrite> = if to_stdout {
        Box::new(try!(Pager::auto(&config, Some("format-patch"))))
    } else {
        Box::new(std::io::stdout())
    };
    let patch_file = |name: &str| -> Result<Box<IoWrite>> {
        println!("{}", name);
        Ok(Box::new(try!(File::create(name))))
    };

    if let Some(ref entry) = cover_entry {
        let cover_blob = try!(repo.find_blob(entry.id()));
        let content = try!(std::str::from_utf8(cover_blob.content())).to_string();
        let (subject, body) = split_message(&content);

        let series_tree = try!(repo.find_commit(series.id())).tree().unwrap();
        let base_tree = try!(repo.find_commit(base.id())).tree().unwrap();
        let diff = try!(repo.diff_tree_to_tree(Some(&base_tree), Some(&series_tree), None));
        let stats = try!(diffstat(&diff));

        if !to_stdout {
            out = try!(patch_file("0000-cover-letter.patch"));
        }
        try!(writeln!(out, "From {} Mon Sep 17 00:00:00 2001", shead_commit.id()));
        try!(writeln!(out, "Message-Id: {}", root_message_id));
        try!(writeln!(out, "From: {} <{}>", author_name, author_email));
        try!(writeln!(out, "Date: {}", date_822(author.when())));
        try!(writeln!(out, "Subject: [PATCH 0/{}] {}\n", commits.len(), subject));
        if !body.is_empty() {
            try!(writeln!(out, "{}", body));
        }
        try!(writeln!(out, "{}", shortlog(&mut commits)));
        try!(writeln!(out, "{}", stats));
        try!(writeln!(out, "{}", signature));
    }

    let mut need_sep = cover_entry.is_some();
    for (commit_num, commit) in commits.iter().enumerate() {
        if !need_sep {
            need_sep = true;
        } else if to_stdout {
            try!(writeln!(out, ""));
        }

        let message = commit.message().unwrap();
        let (subject, body) = split_message(message);
        let commit_id = commit.id();
        let commit_author = commit.author();
        let summary_sanitized = sanitize_summary(&subject);
        let message_id = format!("<{}.{}>", commit_id, message_id_suffix);
        let parent = try!(commit.parent(0));
        let diff = try!(repo.diff_tree_to_tree(Some(&parent.tree().unwrap()), Some(&commit.tree().unwrap()), None));
        let stats = try!(diffstat(&diff));

        if !to_stdout {
            out = try!(patch_file(&format!("{:04}-{}.patch", commit_num+1, summary_sanitized)));
        }
        try!(writeln!(out, "From {} Mon Sep 17 00:00:00 2001", commit_id));
        try!(writeln!(out, "Message-Id: {}", message_id));
        try!(writeln!(out, "In-Reply-To: {}", root_message_id));
        try!(writeln!(out, "References: {}", root_message_id));
        try!(writeln!(out, "From: {} <{}>", author_name, author_email));
        try!(writeln!(out, "Date: {}", date_822(commit_author.when())));
        try!(writeln!(out, "Subject: [PATCH {}/{}] {}\n", commit_num+1, commits.len(), subject));
        if !body.is_empty() {
            try!(writeln!(out, "{}", body));
        }
        try!(writeln!(out, "---"));
        try!(writeln!(out, "{}", stats));
        try!(write_diff(&mut out, &diff));
        try!(writeln!(out, "{}", signature));
    }

    Ok(())
}

fn log(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let config = try!(repo.config());
    let mut out = try!(Pager::auto(&config, Some("log")));

    let mut revwalk = try!(repo.revwalk());
    revwalk.simplify_first_parent();
    try!(revwalk.push_ref(SHEAD_REF));

    let show_diff = m.is_present("patch");

    for oid in revwalk {
        let oid = try!(oid);
        let commit = try!(repo.find_commit(oid));
        let tree = try!(commit.tree());
        let author = commit.author();

        let first_parent_id = try!(commit.parent_id(0).map_err(|e| format!("Malformed series commit {}: {}", oid, e)));
        let first_series_commit = tree.iter().find(|entry| entry.id() == first_parent_id).is_some();

        try!(writeln!(out, "commit {}", oid));
        try!(writeln!(out, "Author: {} <{}>", author.name().unwrap(), author.email().unwrap()));
        try!(writeln!(out, "Date:   {}\n", date_822(author.when())));
        for line in commit.message().unwrap().lines() {
            try!(writeln!(out, "    {}", line));
        }
        if show_diff {
            writeln!(out, "").unwrap();
            let parent_tree = if first_series_commit {
                None
            } else {
                Some(try!(try!(repo.find_commit(first_parent_id)).tree()))
            };
            let diff = try!(repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None));
            try!(write_diff(&mut out, &diff));
        }

        if first_series_commit {
            break;
        } else {
            try!(writeln!(out, ""));
        }
    }

    Ok(())
}

fn rebase(repo: &Repository, m: &ArgMatches) -> Result<()> {
    match repo.state() {
        git2::RepositoryState::Clean => (),
        git2::RepositoryState::RebaseMerge if repo.path().join("rebase-merge").join("git-series").exists() => {
            return Err("git series rebase already in progress.\nUse \"git rebase --continue\" or \"git rebase --abort\".".into());
        },
        s => { return Err(format!("{:?} in progress; cannot rebase", s).into()); }
    }

    let internals = try!(Internals::read(repo));
    let series = try!(try!(internals.working.get("series")).ok_or("Could not find entry \"series\" in working index"));
    let base = try!(try!(internals.working.get("base")).ok_or("Cannot rebase series; no base set.\nUse \"git series base\" to set base."));
    if series.id() == base.id() {
        return Err("No patches to rebase; series and base identical.".into());
    } else if !try!(repo.graph_descendant_of(series.id(), base.id())) {
        return Err(format!("Cannot rebase: current base {} not an ancestor of series {}", base.id(), series.id()).into());
    }

    // Check for unstaged or uncommitted changes before attempting to rebase.
    let series_commit = try!(repo.find_commit(series.id()));
    let series_tree = try!(series_commit.tree());
    let mut unclean = String::new();
    if !diff_empty(&try!(repo.diff_tree_to_index(Some(&series_tree), None, None))) {
        writeln!(unclean, "Cannot rebase: you have unstaged changes.").unwrap();
    }
    if !diff_empty(&try!(repo.diff_index_to_workdir(None, None))) {
        if unclean.is_empty() {
            writeln!(unclean, "Cannot rebase: your index contains uncommitted changes.").unwrap();
        } else {
            writeln!(unclean, "Additionally, your index contains uncommitted changes.").unwrap();
        }
    }
    if !unclean.is_empty() {
        return Err(unclean.into());
    }

    let mut revwalk = try!(repo.revwalk());
    revwalk.set_sorting(git2::SORT_TOPOLOGICAL|git2::SORT_REVERSE);
    try!(revwalk.push(series.id()));
    try!(revwalk.hide(base.id()));
    let commits: Vec<Commit> = try!(revwalk.map(|c| {
        let id = try!(c);
        let mut commit = try!(repo.find_commit(id));
        if commit.parent_ids().count() > 1 {
            return Err(format!("Error: cannot rebase merge commit:\n{}", try!(commit_obj_summarize(&mut commit))).into());
        }
        Ok(commit)
    }).collect::<Result<_>>());

    let interactive = m.is_present("interactive");
    let onto = match m.value_of("onto") {
        None => None,
        Some(onto) => {
            let obj = try!(repo.revparse_single(onto));
            Some(obj.id())
        },
    };

    let newbase = onto.unwrap_or(base.id());
    let (base_short, _) = try!(commit_summarize_components(&repo, base.id()));
    let (newbase_short, _) = try!(commit_summarize_components(&repo, newbase));
    let (series_short, _) = try!(commit_summarize_components(&repo, series.id()));

    let newbase_obj = try!(repo.find_commit(newbase)).into_object();

    let dir = try!(TempDir::new_in(repo.path(), "rebase-merge"));
    let final_path = repo.path().join("rebase-merge");
    let mut create = std::fs::OpenOptions::new();
    create.write(true).create_new(true);

    try!(create.open(dir.path().join("git-series")));
    try!(create.open(dir.path().join("quiet")));
    try!(create.open(dir.path().join("interactive")));

    let mut head_name_file = try!(create.open(dir.path().join("head-name")));
    try!(writeln!(head_name_file, "detached HEAD"));

    let mut onto_file = try!(create.open(dir.path().join("onto")));
    try!(writeln!(onto_file, "{}", newbase));

    let mut orig_head_file = try!(create.open(dir.path().join("orig-head")));
    try!(writeln!(orig_head_file, "{}", series.id()));

    let git_rebase_todo_filename = dir.path().join("git-rebase-todo");
    let mut git_rebase_todo = try!(create.open(&git_rebase_todo_filename));
    for mut commit in commits {
        try!(writeln!(git_rebase_todo, "pick {}", try!(commit_obj_summarize(&mut commit))));
    }
    if let Some(onto) = onto {
        try!(writeln!(git_rebase_todo, "exec git series base {}", onto));
    }
	try!(writeln!(git_rebase_todo, "\n# Rebase {}..{} onto {}", base_short, series_short, newbase_short));
    try!(write!(git_rebase_todo, "{}", REBASE_COMMENT));
    drop(git_rebase_todo);

    // Interactive editor if interactive {
    if interactive {
        let config = try!(repo.config());
        try!(run_editor(&config, &git_rebase_todo_filename));
        let mut file = try!(File::open(&git_rebase_todo_filename));
        let mut todo = String::new();
        try!(file.read_to_string(&mut todo));
        let todo = try!(git2::message_prettify(todo, git2::DEFAULT_COMMENT_CHAR));
        if todo.is_empty() {
            return Err("Nothing to do".into());
        }
    }

    // Avoid races by not calling .into_path until after the rename succeeds.
    try!(std::fs::rename(dir.path(), final_path));
    dir.into_path();

    try!(checkout_tree(repo, &newbase_obj));
    try!(repo.reference("HEAD", newbase, true, &format!("rebase -i (start): checkout {}", newbase)));

    let status = try!(Command::new("git").arg("rebase").arg("--continue").status());
    if !status.success() {
        return Err(format!("git rebase --continue exited with status {}", status).into());
    }

    Ok(())
}

fn req(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let config = try!(repo.config());
    let shead = try!(repo.find_reference(SHEAD_REF));
    let shead_commit = try!(peel_to_commit(try!(shead.resolve())));
    let stree = try!(shead_commit.tree());

    let series = try!(stree.get_name("series").ok_or("Internal error: series did not contain \"series\""));
    let series_id = series.id();
    let mut series_commit = try!(repo.find_commit(series_id));
    let base = try!(stree.get_name("base").ok_or("Cannot request pull; no base set.\nUse \"git series base\" to set base."));
    let mut base_commit = try!(repo.find_commit(base.id()));

    let (cover_content, subject, cover_body) = if let Some(entry) = stree.get_name("cover") {
        let cover_blob = try!(repo.find_blob(entry.id()));
        let content = try!(std::str::from_utf8(cover_blob.content())).to_string();
        let (subject, body) = split_message(&content);
        (Some(content.to_string()), subject.to_string(), Some(body.to_string()))
    } else {
        (None, try!(shead_series_name(&shead)), None)
    };

    let url = m.value_of("url").unwrap();
    let tag = m.value_of("tag").unwrap();
    let full_tag = format!("refs/tags/{}", tag);
    let full_tag_peeled = format!("{}^{{}}", full_tag);
    let full_head = format!("refs/heads/{}", tag);
    let mut remote = try!(repo.remote_anonymous(url));
    try!(remote.connect(git2::Direction::Fetch).map_err(|e| format!("Could not connect to remote repository {}\n{}", url, e)));
    let remote_heads = try!(remote.list());

    /* Find the requested name as either a tag or head */
    let mut opt_remote_tag = None;
    let mut opt_remote_tag_peeled = None;
    let mut opt_remote_head = None;
    for h in remote_heads {
        if h.name() == full_tag {
            opt_remote_tag = Some(h.oid());
        } else if h.name() == full_tag_peeled {
            opt_remote_tag_peeled = Some(h.oid());
        } else if h.name() == full_head {
            opt_remote_head = Some(h.oid());
        }
    }
    let (msg, extra_body, remote_pull_name) = match (opt_remote_tag, opt_remote_tag_peeled, opt_remote_head) {
        (Some(remote_tag), Some(remote_tag_peeled), _) => {
            if remote_tag_peeled != series_id {
                return Err(format!("Remote tag {} does not refer to series {}", tag, series_id).into());
            }
            let local_tag = try!(repo.find_tag(remote_tag).map_err(|e|
                    format!("Could not find remote tag {} ({}) in local repository: {}", tag, remote_tag, e)));
            let mut local_tag_msg = local_tag.message().unwrap().to_string();
            if let Some(sig_index) = local_tag_msg.find("-----BEGIN PGP ") {
                local_tag_msg.truncate(sig_index);
            }
            let extra_body = match cover_content {
                Some(ref content) if !local_tag_msg.contains(content) => cover_body,
                _ => None,
            };
            (Some(local_tag_msg), extra_body, full_tag)
        },
        (Some(remote_tag), None, _) => {
            if remote_tag != series_id {
                return Err(format!("Remote unannotated tag {} does not refer to series {}", tag, series_id).into());
            }
            (cover_content, None, full_tag)
        }
        (_, _, Some(remote_head)) => {
            if remote_head != series_id {
                return Err(format!("Remote branch {} does not refer to series {}", tag, series_id).into());
            }
            (cover_content, None, full_head)
        },
        _ => {
            return Err(format!("Remote does not have either a tag or branch named {}", tag).into())
        }
    };

    let commit_subject_date = |commit: &mut Commit| -> String {
        let date = date_822(commit.author().when());
        let summary = commit.summary().unwrap();
        format!("  {} ({})", summary, date)
    };

    let mut revwalk = try!(repo.revwalk());
    revwalk.set_sorting(git2::SORT_TOPOLOGICAL|git2::SORT_REVERSE);
    try!(revwalk.push(series_id));
    try!(revwalk.hide(base.id()));
    let mut commits: Vec<Commit> = try!(revwalk.map(|c| {
        Ok(try!(repo.find_commit(try!(c))))
    }).collect::<Result<_>>());
    if commits.is_empty() {
        return Err("No patches to request pull of; series and base identical.".into());
    }

    let author = try!(get_signature(&config, "AUTHOR"));
    let author_email = author.email().unwrap();
    let message_id = format!("<pull.{}.{}.git-series.{}>", shead_commit.id(), author.when().seconds(), author_email);

    let diff = try!(repo.diff_tree_to_tree(Some(&base_commit.tree().unwrap()), Some(&series_commit.tree().unwrap()), None));
    let stats = try!(diffstat(&diff));

    println!("From {} Mon Sep 17 00:00:00 2001", shead_commit.id());
    println!("Message-Id: {}", message_id);
    println!("From: {} <{}>", author.name().unwrap(), author_email);
    println!("Date: {}", date_822(author.when()));
    println!("Subject: [GIT PULL] {}\n", subject);
    if let Some(extra_body) = extra_body {
        println!("{}", extra_body);
    }
    println!("The following changes since commit {}:\n", base.id());
    println!("{}\n", commit_subject_date(&mut base_commit));
    println!("are available in the git repository at:\n");
    println!("  {} {}\n", url, remote_pull_name);
    println!("for you to fetch changes up to {}:\n", series.id());
    println!("{}\n", commit_subject_date(&mut series_commit));
    println!("----------------------------------------------------------------");
    if let Some(msg) = msg {
        println!("{}", msg);
        println!("----------------------------------------------------------------");
    }
    println!("{}", shortlog(&mut commits));
    println!("{}", stats);
    if m.is_present("patch") {
        try!(write_diff(&mut std::io::stdout(), &diff));
    }
    println!("{}", mail_signature());

    Ok(())
}

fn git_series() -> Result<()> {
    let m = App::new("git-series")
            .bin_name("git series")
            .about("Track patch series in git")
            .author("Josh Triplett <josh@joshtriplett.org>")
            .version(crate_version!())
            .global_setting(AppSettings::ColoredHelp)
            .global_setting(AppSettings::VersionlessSubcommands)
            .subcommands(vec![
                SubCommand::with_name("add")
                    .about("Add changes to the index for the next series commit")
                    .arg_from_usage("<change>... 'Changes to add (\"series\", \"base\", \"cover\")'"),
                SubCommand::with_name("base")
                    .about("Get or set the base commit for the patch series")
                    .arg(Arg::with_name("base").help("Base commit").conflicts_with("delete"))
                    .arg_from_usage("-d, --delete 'Clear patch series base'"),
                SubCommand::with_name("checkout")
                    .about("Resume work on a patch series; check out the current version")
                    .arg_from_usage("<name> 'Patch series to check out'"),
                SubCommand::with_name("commit")
                    .about("Record changes to the patch series")
                    .arg_from_usage("-a, --all 'Commit all changes'")
                    .arg_from_usage("-m [msg] 'Commit message'")
                    .arg_from_usage("-v, --verbose 'Show diff when preparing commit message'"),
                SubCommand::with_name("cover")
                    .about("Create or edit the cover letter for the patch series")
                    .arg_from_usage("-d, --delete 'Delete cover letter'"),
                SubCommand::with_name("delete")
                    .about("Delete a patch series")
                    .arg_from_usage("<name> 'Patch series to delete'"),
                SubCommand::with_name("detach")
                    .about("Stop working on any patch series"),
                SubCommand::with_name("format")
                    .arg_from_usage("--stdout 'Write patches to stdout rather than files.")
                    .about("Prepare patch series for email"),
                SubCommand::with_name("log")
                    .about("Show the history of the patch series")
                    .arg_from_usage("-p, --patch 'Include a patch for each change committed to the series'"),
                SubCommand::with_name("rebase")
                    .about("Rebase the patch series")
                    .arg_from_usage("[onto] 'Commit to rebase onto'")
                    .arg_from_usage("-i, --interactive 'Interactively edit the list of commits'")
                    .group(ArgGroup::with_name("action").args(&["onto", "interactive"]).multiple(true).required(true)),
                SubCommand::with_name("req")
                    .about("Generate a mail requesting a pull of the patch series")
                    .visible_aliases(&["pull-request", "request-pull"])
                    .arg_from_usage("-p, --patch 'Include patch in the mail'")
                    .arg_from_usage("<url> 'Repository URL to request pull of'")
                    .arg_from_usage("<tag> 'Tag or branch name to request pull of'"),
                SubCommand::with_name("status")
                    .about("Show the status of the patch series"),
                SubCommand::with_name("start")
                    .about("Start a new patch series")
                    .arg_from_usage("<name> 'Patch series name'"),
                SubCommand::with_name("unadd")
                    .about("Undo \"git series add\", removing changes from the next series commit")
                    .arg_from_usage("<change>... 'Changes to remove (\"series\", \"base\", \"cover\")'"),
            ]).get_matches();

    let repo = try!(git2::Repository::discover("."));

    match m.subcommand() {
        ("", _) => try!(series(&repo)),
        ("add", Some(ref sm)) => try!(add(&repo, &sm)),
        ("base", Some(ref sm)) => try!(base(&repo, &sm)),
        ("checkout", Some(ref sm)) => try!(checkout(&repo, &sm)),
        ("commit", Some(ref sm)) => try!(commit_status(&repo, &sm, false)),
        ("cover", Some(ref sm)) => try!(cover(&repo, &sm)),
        ("delete", Some(ref sm)) => try!(delete(&repo, &sm)),
        ("detach", _) => try!(detach(&repo)),
        ("format", Some(ref sm)) => try!(format(&repo, &sm)),
        ("log", Some(ref sm)) => try!(log(&repo, &sm)),
        ("rebase", Some(ref sm)) => try!(rebase(&repo, &sm)),
        ("req", Some(ref sm)) => try!(req(&repo, &sm)),
        ("start", Some(ref sm)) => try!(start(&repo, &sm)),
        ("status", Some(ref sm)) => try!(commit_status(&repo, &sm, true)),
        ("unadd", Some(ref sm)) => try!(unadd(&repo, &sm)),
        _ => unreachable!()
    }

    Ok(())
}

fn main() {
    if let Err(e) = git_series() {
        match e {
            Error::CommitNoChanges => {},
            Error::CheckoutConflict => {},
            _ => writeln!(std::io::stderr(), "{}", e).unwrap(),
        }
        std::process::exit(1);
    }
}