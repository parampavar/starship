use crate::config::{ModuleConfig, StarshipConfig};
use crate::configs::StarshipRootConfig;
use crate::context_env::Env;
use crate::module::Module;
use crate::utils::{CommandOutput, PathExt, create_command, exec_timeout, read_file};

use crate::modules;
use crate::utils;
use clap::Parser;
use gix::{
    Repository, ThreadSafeRepository,
    repository::Kind,
    sec::{self as git_sec, trust::DefaultForLevel},
    state as git_state,
};
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Debug;
use std::fs;
use std::marker::PhantomData;
use std::num::ParseIntError;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::string::String;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use terminal_size::terminal_size;

/// Context contains data or common methods that may be used by multiple modules.
/// The data contained within Context will be relevant to this particular rendering
/// of the prompt.
pub struct Context<'a> {
    /// The deserialized configuration map from the user's `starship.toml` file.
    pub config: StarshipConfig,

    /// The current working directory that starship is being called in.
    pub current_dir: PathBuf,

    /// A logical directory path which should represent the same directory as `current_dir`,
    /// though may appear different.
    /// E.g. when navigating to a `PSDrive` in `PowerShell`, or a path without symlinks resolved.
    pub logical_dir: PathBuf,

    /// A struct containing directory contents in a lookup-optimized format.
    dir_contents: OnceLock<Result<DirContents, std::io::Error>>,

    /// Properties to provide to modules.
    pub properties: Properties,

    /// Private field to store Git information for modules who need it
    repo: OnceLock<Result<Repo, Box<gix::discover::Error>>>,

    /// The shell the user is assumed to be running
    pub shell: Shell,

    /// Which prompt to print (main, right, ...)
    pub target: Target,

    /// Width of terminal, or zero if width cannot be detected.
    pub width: usize,

    /// A `HashMap` of environment variable mocks
    pub env: Env<'a>,

    /// A `HashMap` of command mocks
    #[cfg(test)]
    pub cmd: HashMap<&'a str, Option<CommandOutput>>,

    /// a mock of the root directory
    #[cfg(test)]
    pub root_dir: tempfile::TempDir,

    #[cfg(feature = "battery")]
    pub battery_info_provider: &'a (dyn crate::modules::BatteryInfoProvider + Send + Sync),

    /// Starship root config
    pub root_config: StarshipRootConfig,

    /// Avoid issues with unused lifetimes when features are disabled
    _marker: PhantomData<&'a ()>,
}

impl<'a> Context<'a> {
    /// Identify the current working directory and create an instance of Context
    /// for it. "logical-path" is used when a shell allows the "current working directory"
    /// to be something other than a file system path (like powershell provider specific paths).
    pub fn new(arguments: Properties, target: Target) -> Self {
        let shell = Context::get_shell();

        // Retrieve the "current directory".
        // If the path argument is not set fall back to the OS current directory.
        let path = arguments
            .path
            .clone()
            .or_else(|| env::current_dir().ok())
            .or_else(|| env::var("PWD").map(PathBuf::from).ok())
            .or_else(|| arguments.logical_path.clone())
            .unwrap_or_default();

        // Retrieve the "logical directory".
        // If the path argument is not set fall back to the PWD env variable set by many shells
        // or to the other path.
        let logical_path = arguments
            .logical_path
            .clone()
            .or_else(|| env::var("PWD").map(PathBuf::from).ok())
            .unwrap_or_else(|| path.clone());

        Self::new_with_shell_and_path(
            arguments,
            shell,
            target,
            path,
            logical_path,
            Default::default(),
        )
    }

    /// Create a new instance of Context for the provided directory
    pub fn new_with_shell_and_path(
        mut properties: Properties,
        shell: Shell,
        target: Target,
        path: PathBuf,
        logical_path: PathBuf,
        env: Env<'a>,
    ) -> Self {
        let config = StarshipConfig::initialize(&get_config_path_os(&env));

        // If the vector is zero-length, we should pretend that we didn't get a
        // pipestatus at all (since this is the input `--pipestatus=""`)
        if properties
            .pipestatus
            .as_deref()
            .is_some_and(|p| p.len() == 1 && p[0].is_empty())
        {
            properties.pipestatus = None;
        }
        log::trace!(
            "Received completed pipestatus of {:?}",
            properties.pipestatus
        );

        // If status-code is empty, set it to None
        if matches!(properties.status_code.as_deref(), Some("")) {
            properties.status_code = None;
        }

        // Canonicalize the current path to resolve symlinks, etc.
        // NOTE: On Windows this may convert the path to extended-path syntax.
        let current_dir = Context::expand_tilde(path);
        let current_dir = dunce::canonicalize(&current_dir).unwrap_or(current_dir);
        let logical_dir = logical_path;

        let root_config = config
            .config
            .as_ref()
            .map_or_else(StarshipRootConfig::default, StarshipRootConfig::load);

        let width = properties.terminal_width;

        Self {
            config,
            properties,
            current_dir,
            logical_dir,
            dir_contents: OnceLock::new(),
            repo: OnceLock::new(),
            shell,
            target,
            width,
            env,
            #[cfg(test)]
            root_dir: tempfile::TempDir::new().unwrap(),
            #[cfg(test)]
            cmd: HashMap::new(),
            #[cfg(feature = "battery")]
            battery_info_provider: &crate::modules::BatteryInfoProviderImpl,
            root_config,
            _marker: PhantomData,
        }
    }

    /// Sets the context config, overwriting the existing config
    pub fn set_config(mut self, config: toml::Table) -> Self {
        self.root_config = StarshipRootConfig::load(&config);
        self.config = StarshipConfig {
            config: Some(config),
        };
        self
    }

    // Tries to retrieve home directory from a table in testing mode or else retrieves it from the os
    pub fn get_home(&self) -> Option<PathBuf> {
        home_dir(&self.env)
    }

    // Retrieves a environment variable from the os or from a table if in testing mode
    #[inline]
    pub fn get_env<K: AsRef<str>>(&self, key: K) -> Option<String> {
        self.env.get_env(key)
    }

    // Retrieves a environment variable from the os or from a table if in testing mode (os version)
    #[inline]
    pub fn get_env_os<K: AsRef<str>>(&self, key: K) -> Option<OsString> {
        self.env.get_env_os(key)
    }

    /// Convert a `~` in a path to the home directory
    pub fn expand_tilde(dir: PathBuf) -> PathBuf {
        if dir.starts_with("~") {
            let without_home = dir.strip_prefix("~").unwrap();
            return utils::home_dir().unwrap().join(without_home);
        }
        dir
    }

    /// Create a new module
    pub fn new_module(&self, name: &str) -> Module {
        let config = self.config.get_module_config(name);
        let desc = modules::description(name);

        Module::new(name, desc, config)
    }

    /// Check if `disabled` option of the module is true in configuration file.
    pub fn is_module_disabled_in_config(&self, name: &str) -> bool {
        let config = self.config.get_module_config(name);

        // If the segment has "disabled" set to "true", don't show it
        let disabled = config.and_then(|table| table.as_table()?.get("disabled")?.as_bool());

        disabled == Some(true)
    }

    /// Returns true when a negated environment variable is defined in `env_vars` and is present
    fn has_negated_env_var(&self, env_vars: &'a [&'a str]) -> bool {
        env_vars
            .iter()
            .filter_map(|env_var| env_var.strip_prefix('!'))
            .any(|env_var| self.get_env(env_var).is_some())
    }

    /// Returns true if `detect_env_vars` is empty,
    /// or if at least one environment variable is set and no negated environment variable is set
    pub fn detect_env_vars(&'a self, env_vars: &'a [&'a str]) -> bool {
        if env_vars.is_empty() {
            return true;
        }

        if self.has_negated_env_var(env_vars) {
            return false;
        }

        // Returns true if at least one environment variable is set
        let mut iter = env_vars
            .iter()
            .filter(|env_var| !env_var.starts_with('!'))
            .peekable();

        iter.peek().is_none() || iter.any(|env_var| self.get_env(env_var).is_some())
    }

    // returns a new ScanDir struct with reference to current dir_files of context
    // see ScanDir for methods
    pub fn try_begin_scan(&'a self) -> Option<ScanDir<'a>> {
        Some(ScanDir {
            dir_contents: self.dir_contents().ok()?,
            files: &[],
            folders: &[],
            extensions: &[],
        })
    }

    /// Begins an ancestor scan at the current directory, see [`ScanAncestors`] for available
    /// methods.
    pub fn begin_ancestor_scan(&'a self) -> ScanAncestors<'a> {
        ScanAncestors {
            path: &self.current_dir,
            files: &[],
            folders: &[],
        }
    }

    /// Will lazily get repo root and branch when a module requests it.
    pub fn get_repo(&self) -> Result<&Repo, &gix::discover::Error> {
        self.repo
            .get_or_init(|| -> Result<Repo, Box<gix::discover::Error>> {
                // custom open options
                let mut git_open_opts_map =
                    git_sec::trust::Mapping::<gix::open::Options>::default();

                // Load all the configuration as it affects aspects of the
                // `git_status` and `git_metrics` modules.
                let config = gix::open::permissions::Config {
                    git_binary: true,
                    system: true,
                    git: true,
                    user: true,
                    env: true,
                    includes: true,
                };
                // change options for config permissions without touching anything else
                git_open_opts_map.reduced =
                    git_open_opts_map
                        .reduced
                        .permissions(gix::open::Permissions {
                            config,
                            ..gix::open::Permissions::default_for_level(git_sec::Trust::Reduced)
                        });
                git_open_opts_map.full =
                    git_open_opts_map.full.permissions(gix::open::Permissions {
                        config,
                        ..gix::open::Permissions::default_for_level(git_sec::Trust::Full)
                    });

                let shared_repo =
                    match ThreadSafeRepository::discover_with_environment_overrides_opts(
                        &self.current_dir,
                        gix::discover::upwards::Options {
                            match_ceiling_dir_or_error: false,
                            ..Default::default()
                        },
                        git_open_opts_map,
                    ) {
                        Ok(repo) => repo,
                        Err(e) => {
                            log::debug!("Failed to find git repo: {e}");
                            return Err(Box::new(e));
                        }
                    };

                let repository = shared_repo.to_thread_local();
                log::trace!(
                    "Found git repo: {repository:?}, (trust: {:?})",
                    repository.git_dir_trust()
                );

                let branch = get_current_branch(&repository);
                let remote =
                    get_remote_repository_info(&repository, branch.as_ref().map(AsRef::as_ref));
                let path = repository.path().to_path_buf();

                let fs_monitor_value_is_true = repository
                    .config_snapshot()
                    .boolean("core.fsmonitor")
                    .unwrap_or(false);

                Ok(Repo {
                    repo: shared_repo,
                    branch: branch.map(|b| b.shorten().to_string()),
                    workdir: repository.workdir().map(PathBuf::from),
                    path,
                    state: repository.state(),
                    remote,
                    fs_monitor_value_is_true,
                    kind: repository.kind(),
                })
            })
            .as_ref()
            .map_err(std::convert::AsRef::as_ref)
    }

    pub fn dir_contents(&self) -> Result<&DirContents, &std::io::Error> {
        self.dir_contents
            .get_or_init(|| {
                let timeout = self.root_config.scan_timeout;
                DirContents::from_path_with_timeout(
                    &self.current_dir,
                    Duration::from_millis(timeout),
                    self.root_config.follow_symlinks,
                )
            })
            .as_ref()
    }

    fn get_shell() -> Shell {
        let shell = env::var("STARSHIP_SHELL").unwrap_or_default();
        match shell.as_str() {
            "bash" => Shell::Bash,
            "fish" => Shell::Fish,
            "ion" => Shell::Ion,
            "pwsh" => Shell::Pwsh,
            "powershell" => Shell::PowerShell,
            "zsh" => Shell::Zsh,
            "elvish" => Shell::Elvish,
            "tcsh" => Shell::Tcsh,
            "nu" => Shell::Nu,
            "xonsh" => Shell::Xonsh,
            "cmd" => Shell::Cmd,
            _ => Shell::Unknown,
        }
    }

    // TODO: This should be used directly by clap parse
    pub fn get_cmd_duration(&self) -> Option<u128> {
        self.properties
            .cmd_duration
            .as_deref()
            .and_then(|cd| cd.parse::<u128>().ok())
    }

    /// Execute a command and return the output on stdout and stderr if successful
    #[inline]
    pub fn exec_cmd<T: AsRef<OsStr> + Debug, U: AsRef<OsStr> + Debug>(
        &self,
        cmd: T,
        args: &[U],
    ) -> Option<CommandOutput> {
        log::trace!("Executing command {cmd:?} with args {args:?} from context");
        #[cfg(test)]
        {
            let command = crate::utils::display_command(&cmd, args);
            if let Some(output) = self
                .cmd
                .get(command.as_str())
                .cloned()
                .or_else(|| crate::utils::mock_cmd(&cmd, args))
            {
                return output;
            }
        }
        let mut cmd = create_command(cmd).ok()?;
        cmd.args(args).current_dir(&self.current_dir);
        exec_timeout(
            &mut cmd,
            Duration::from_millis(self.root_config.command_timeout),
        )
    }

    /// Attempt to execute several commands with `exec_cmd`, return the results of the first that works
    pub fn exec_cmds_return_first(&self, commands: Vec<Vec<&str>>) -> Option<CommandOutput> {
        commands
            .iter()
            .find_map(|attempt| self.exec_cmd(attempt[0], &attempt[1..]))
    }

    /// Returns the string contents of a file from the current working directory
    pub fn read_file_from_pwd(&self, file_name: &str) -> Option<String> {
        if !self.try_begin_scan()?.set_files(&[file_name]).is_match() {
            log::debug!(
                "Not attempting to read {file_name} because, it was not found during scan."
            );
            return None;
        }

        read_file(self.current_dir.join(file_name)).ok()
    }

    pub fn get_config_path_os(&self) -> Option<OsString> {
        get_config_path_os(&self.env)
    }
}

impl Default for Context<'_> {
    fn default() -> Self {
        Self::new(Default::default(), Target::Main)
    }
}

fn home_dir(env: &Env) -> Option<PathBuf> {
    if cfg!(test) {
        if let Some(home) = env.get_env("HOME") {
            return Some(PathBuf::from(home));
        }
    }
    utils::home_dir()
}

fn get_config_path_os(env: &Env) -> Option<OsString> {
    if let Some(config_path) = env.get_env_os("STARSHIP_CONFIG") {
        return Some(config_path);
    }
    Some(home_dir(env)?.join(".config").join("starship.toml").into())
}

#[derive(Debug)]
pub struct DirContents {
    // HashSet of all files, no folders, relative to the base directory given at construction.
    files: HashSet<PathBuf>,
    // HashSet of all file names, e.g. the last section without any folders, as strings.
    file_names: HashSet<String>,
    // HashSet of all folders, relative to the base directory given at construction.
    folders: HashSet<PathBuf>,
    // HashSet of all extensions found, without dots, e.g. "js" instead of ".js".
    extensions: HashSet<String>,
}

impl DirContents {
    #[cfg(test)]
    fn from_path(base: &Path, follow_symlinks: bool) -> Result<Self, std::io::Error> {
        Self::from_path_with_timeout(base, Duration::from_secs(30), follow_symlinks)
    }

    fn from_path_with_timeout(
        base: &Path,
        timeout: Duration,
        follow_symlinks: bool,
    ) -> Result<Self, std::io::Error> {
        let start = Instant::now();

        let mut folders: HashSet<PathBuf> = HashSet::new();
        let mut files: HashSet<PathBuf> = HashSet::new();
        let mut file_names: HashSet<String> = HashSet::new();
        let mut extensions: HashSet<String> = HashSet::new();

        fs::read_dir(base)?
            .enumerate()
            .take_while(|(n, _)| {
                cfg!(test) // ignore timeout during tests
                || n & 0xFF != 0 // only check timeout once every 2^8 entries
                || start.elapsed() < timeout
            })
            .filter_map(|(_, entry)| entry.ok())
            .for_each(|entry| {
                let path = PathBuf::from(entry.path().strip_prefix(base).unwrap());

                let is_dir = match follow_symlinks {
                    true => entry.path().is_dir(),
                    false => fs::symlink_metadata(entry.path())
                        .map(|m| m.is_dir())
                        .unwrap_or(false),
                };

                if is_dir {
                    folders.insert(path);
                } else {
                    if !path.to_string_lossy().starts_with('.') {
                        // Extract the file extensions (yes, that's plural) from a filename.
                        // Why plural? Consider the case of foo.tar.gz. It's a compressed
                        // tarball (tar.gz), and it's a gzipped file (gz). We should be able
                        // to match both.

                        // find the minimal extension on a file. ie, the gz in foo.tar.gz
                        // NB the .to_string_lossy().to_string() here looks weird but is
                        // required to convert it from a Cow.
                        path.extension()
                            .map(|ext| extensions.insert(ext.to_string_lossy().to_string()));

                        // find the full extension on a file. ie, the tar.gz in foo.tar.gz
                        path.file_name().map(|file_name| {
                            file_name
                                .to_string_lossy()
                                .split_once('.')
                                .map(|(_, after)| extensions.insert(after.to_string()))
                        });
                    }
                    if let Some(file_name) = path.file_name() {
                        // this .to_string_lossy().to_string() is also required
                        file_names.insert(file_name.to_string_lossy().to_string());
                    }
                    files.insert(path);
                }
            });

        log::trace!(
            "Building HashSets of directory files, folders and extensions took {:?}",
            start.elapsed()
        );

        Ok(Self {
            files,
            file_names,
            folders,
            extensions,
        })
    }

    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter()
    }

    pub fn has_file(&self, path: &str) -> bool {
        self.files.contains(Path::new(path))
    }

    pub fn has_file_name(&self, name: &str) -> bool {
        self.file_names.contains(name)
    }

    pub fn has_folder(&self, path: &str) -> bool {
        self.folders.contains(Path::new(path))
    }

    pub fn has_extension(&self, ext: &str) -> bool {
        self.extensions.contains(ext)
    }

    pub fn has_any_positive_file_name(&self, names: &[&str]) -> bool {
        names
            .iter()
            .any(|name| !name.starts_with('!') && self.has_file_name(name))
    }

    pub fn has_any_positive_folder(&self, paths: &[&str]) -> bool {
        paths
            .iter()
            .any(|path| !path.starts_with('!') && self.has_folder(path))
    }

    pub fn has_any_positive_extension(&self, exts: &[&str]) -> bool {
        exts.iter()
            .any(|ext| !ext.starts_with('!') && self.has_extension(ext))
    }

    pub fn has_no_negative_file_name(&self, names: &[&str]) -> bool {
        !names
            .iter()
            .any(|name| name.starts_with('!') && self.has_file_name(&name[1..]))
    }

    pub fn has_no_negative_folder(&self, paths: &[&str]) -> bool {
        !paths
            .iter()
            .any(|path| path.starts_with('!') && self.has_folder(&path[1..]))
    }

    pub fn has_no_negative_extension(&self, exts: &[&str]) -> bool {
        !exts
            .iter()
            .any(|ext| ext.starts_with('!') && self.has_extension(&ext[1..]))
    }
}

pub struct Repo {
    pub repo: ThreadSafeRepository,

    /// If `current_dir` is a git repository or is contained within one,
    /// this is the short name of the current branch name of that repo,
    /// i.e. `main`.
    pub branch: Option<String>,

    /// If `current_dir` is a git repository or is contained within one,
    /// this is the path to the root of that repo.
    pub workdir: Option<PathBuf>,

    /// The path of the repository's `.git` directory.
    pub path: PathBuf,

    /// State
    pub state: Option<git_state::InProgress>,

    /// Remote repository
    pub remote: Option<Remote>,

    /// Contains `true` if the value of `core.fsmonitor` is set to `true`.
    /// If not `true`, `fsmonitor` is explicitly disabled in git commands.
    pub(crate) fs_monitor_value_is_true: bool,

    // Kind of repository, work tree or bare
    pub kind: Kind,
}

impl Repo {
    /// Opens the associated git repository.
    pub fn open(&self) -> Repository {
        self.repo.to_thread_local()
    }

    /// Wrapper to execute external git commands.
    /// Handles adding the appropriate `--git-dir` and `--work-tree` flags to the command.
    /// Also handles additional features required for security, such as disabling `fsmonitor`.
    /// At this time, mocking is not supported.
    pub fn exec_git<T: AsRef<OsStr> + Debug>(
        &self,
        context: &Context,
        git_args: impl IntoIterator<Item = T>,
    ) -> Option<CommandOutput> {
        let mut command = create_command("git").ok()?;

        // A value of `true` should not execute external commands.
        let fsm_config_value = if self.fs_monitor_value_is_true {
            "core.fsmonitor=true"
        } else {
            "core.fsmonitor="
        };

        command.env("GIT_OPTIONAL_LOCKS", "0").args([
            OsStr::new("-C"),
            context.current_dir.as_os_str(),
            OsStr::new("--git-dir"),
            self.path.as_os_str(),
            OsStr::new("-c"),
            OsStr::new(fsm_config_value),
        ]);

        // Bare repositories might not have a workdir, so we need to check for that.
        if let Some(wt) = self.workdir.as_ref() {
            command.args([OsStr::new("--work-tree"), wt.as_os_str()]);
        }

        command.args(git_args);
        log::trace!("Executing git command: {command:?}");

        exec_timeout(
            &mut command,
            Duration::from_millis(context.root_config.command_timeout),
        )
    }
}

/// Remote repository
pub struct Remote {
    pub branch: Option<String>,
    pub name: Option<String>,
}

// A struct of Criteria which will be used to verify current PathBuf is
// of X language, criteria can be set via the builder pattern
pub struct ScanDir<'a> {
    dir_contents: &'a DirContents,
    files: &'a [&'a str],
    folders: &'a [&'a str],
    extensions: &'a [&'a str],
}

impl<'a> ScanDir<'a> {
    #[must_use]
    pub const fn set_files(mut self, files: &'a [&'a str]) -> Self {
        self.files = files;
        self
    }

    #[must_use]
    pub const fn set_extensions(mut self, extensions: &'a [&'a str]) -> Self {
        self.extensions = extensions;
        self
    }

    #[must_use]
    pub const fn set_folders(mut self, folders: &'a [&'a str]) -> Self {
        self.folders = folders;
        self
    }

    /// based on the current `PathBuf` check to see
    /// if any of this criteria match or exist and returning a boolean
    pub fn is_match(&self) -> bool {
        // if there exists a file with a file/folder/ext we've said we don't want,
        // fail the match straight away
        self.dir_contents.has_no_negative_extension(self.extensions)
            && self.dir_contents.has_no_negative_file_name(self.files)
            && self.dir_contents.has_no_negative_folder(self.folders)
            && (self
                .dir_contents
                .has_any_positive_extension(self.extensions)
                || self.dir_contents.has_any_positive_file_name(self.files)
                || self.dir_contents.has_any_positive_folder(self.folders))
    }
}

/// Scans the ancestors of a given path until a directory containing one of the given files or
/// folders is found.
pub struct ScanAncestors<'a> {
    path: &'a Path,
    files: &'a [&'a str],
    folders: &'a [&'a str],
}

impl<'a> ScanAncestors<'a> {
    #[must_use]
    pub const fn set_files(mut self, files: &'a [&'a str]) -> Self {
        self.files = files;
        self
    }

    #[must_use]
    pub const fn set_folders(mut self, folders: &'a [&'a str]) -> Self {
        self.folders = folders;
        self
    }

    /// Scans upwards starting from the initial path until a directory containing one of the given
    /// files or folders is found.
    ///
    /// The scan does not cross device boundaries.
    pub fn scan(&self) -> Option<PathBuf> {
        let path = self.path;
        let initial_device_id = path.device_id();

        // We want to avoid reallocations during the search so we pre-allocate a buffer with enough
        // capacity to hold the longest path + any marker (we could find the length of the longest
        // marker programmatically but that would actually cost more than preallocating a few bytes too many).
        let mut buf = PathBuf::with_capacity(path.as_os_str().len() + 15);
        path.clone_into(&mut buf);

        loop {
            if initial_device_id != buf.device_id() {
                break;
            }

            for file in self.files {
                // Then for each file, we look for `buf/file`
                buf.push(file);

                if buf.is_file() {
                    buf.pop();
                    return Some(buf);
                }

                // Removing the last pushed item means removing `file`, to replace it with either
                // the next `file` or the first `folder`, if any
                buf.pop();
            }

            for folder in self.folders {
                // Then for each folder, we look for `buf/folder`
                buf.push(folder);

                if buf.is_dir() {
                    buf.pop();
                    return Some(buf);
                }

                buf.pop();
            }

            // Then we go up one level until there is no more level to go up with
            if !buf.pop() {
                break;
            }
        }

        None
    }
}

fn get_current_branch(repository: &Repository) -> Option<gix::refs::FullName> {
    repository.head_name().ok()?
}

fn get_remote_repository_info(
    repository: &Repository,
    branch_name: Option<&gix::refs::FullNameRef>,
) -> Option<Remote> {
    let branch_name = branch_name?;
    let branch = repository
        .branch_remote_ref_name(branch_name, gix::remote::Direction::Fetch)
        .and_then(std::result::Result::ok)
        .map(|r| r.shorten().to_string());
    let name = repository
        .branch_remote_name(branch_name.shorten(), gix::remote::Direction::Fetch)
        .map(|n| n.as_bstr().to_string());

    Some(Remote { branch, name })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Fish,
    Ion,
    Pwsh,
    PowerShell,
    Zsh,
    Elvish,
    Tcsh,
    Nu,
    Xonsh,
    Cmd,
    Unknown,
}

/// Which kind of prompt target to print (main prompt, rprompt, ...)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Main,
    Right,
    Continuation,
    Profile(String),
}

/// Properties as passed on from the shell as arguments
#[derive(Parser, Debug)]
pub struct Properties {
    /// The status code of the previously run command as an unsigned or signed 32bit integer
    #[clap(short = 's', long = "status")]
    pub status_code: Option<String>,
    /// Bash, Fish and Zsh support returning codes for each process in a pipeline.
    #[clap(long, value_delimiter = ' ')]
    pub pipestatus: Option<Vec<String>>,
    /// The width of the current interactive terminal.
    #[clap(short = 'w', long, default_value_t=default_width(), value_parser=parse_width)]
    terminal_width: usize,
    /// The path that the prompt should render for.
    #[clap(short, long)]
    path: Option<PathBuf>,
    /// The logical path that the prompt should render for.
    /// This path should be a virtual/logical representation of the PATH argument.
    #[clap(short = 'P', long)]
    logical_path: Option<PathBuf>,
    /// The execution duration of the last command, in milliseconds
    #[clap(short = 'd', long)]
    pub cmd_duration: Option<String>,
    /// The keymap of fish/zsh/cmd
    #[clap(short = 'k', long, default_value = "viins")]
    pub keymap: String,
    /// The number of currently running jobs
    #[clap(short, long, default_value_t, value_parser=parse_i64)]
    pub jobs: i64,
    /// The current value of SHLVL, for shells that mis-handle it in $()
    #[clap(long, value_parser=parse_i64)]
    pub shlvl: Option<i64>,
}

impl Default for Properties {
    fn default() -> Self {
        Self {
            status_code: None,
            pipestatus: None,
            terminal_width: default_width(),
            path: None,
            logical_path: None,
            cmd_duration: None,
            keymap: "viins".to_string(),
            jobs: 0,
            shlvl: None,
        }
    }
}

/// Parse String, but treat empty strings as `None`
fn parse_trim<F: FromStr>(value: &str) -> Option<Result<F, F::Err>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(F::from_str(value))
}

fn parse_i64(value: &str) -> Result<i64, ParseIntError> {
    parse_trim(value).unwrap_or(Ok(0))
}

fn default_width() -> usize {
    terminal_size().map_or(80, |(w, _)| w.0 as usize)
}

fn parse_width(width: &str) -> Result<usize, ParseIntError> {
    parse_trim(width).unwrap_or_else(|| Ok(default_width()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::default_context;
    use std::io;

    fn testdir(paths: &[&str]) -> Result<tempfile::TempDir, std::io::Error> {
        let dir = tempfile::tempdir()?;
        for path in paths {
            let p = dir.path().join(Path::new(path));
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::File::create(p)?.sync_all()?;
        }
        Ok(dir)
    }

    #[test]
    fn test_scan_dir_no_symlinks() -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(not(target_os = "windows"))]
        use std::os::unix::fs::symlink;
        #[cfg(target_os = "windows")]
        use std::os::windows::fs::symlink_dir as symlink;

        let d = testdir(&["file"])?;
        fs::create_dir(d.path().join("folder"))?;

        symlink(d.path().join("folder"), d.path().join("link_to_folder"))?;
        symlink(d.path().join("file"), d.path().join("link_to_file"))?;

        let dc_following_symlinks = DirContents::from_path(d.path(), true)?;

        assert!(
            ScanDir {
                dir_contents: &dc_following_symlinks,
                files: &["link_to_file"],
                extensions: &[],
                folders: &[],
            }
            .is_match()
        );

        assert!(
            ScanDir {
                dir_contents: &dc_following_symlinks,
                files: &[],
                extensions: &[],
                folders: &["link_to_folder"],
            }
            .is_match()
        );

        let dc_not_following_symlinks = DirContents::from_path(d.path(), false)?;

        assert!(
            ScanDir {
                dir_contents: &dc_not_following_symlinks,
                files: &["link_to_file"],
                extensions: &[],
                folders: &[],
            }
            .is_match()
        );

        assert!(
            !ScanDir {
                dir_contents: &dc_not_following_symlinks,
                files: &[],
                extensions: &[],
                folders: &["link_to_folder"],
            }
            .is_match()
        );

        Ok(())
    }

    #[test]
    fn test_scan_dir() -> Result<(), Box<dyn std::error::Error>> {
        let empty = testdir(&[])?;
        let follow_symlinks = true;
        let empty_dc = DirContents::from_path(empty.path(), follow_symlinks)?;

        assert!(
            !ScanDir {
                dir_contents: &empty_dc,
                files: &["package.json"],
                extensions: &["js"],
                folders: &["node_modules"],
            }
            .is_match()
        );
        empty.close()?;

        let rust = testdir(&["README.md", "Cargo.toml", "src/main.rs"])?;
        let rust_dc = DirContents::from_path(rust.path(), follow_symlinks)?;
        assert!(
            !ScanDir {
                dir_contents: &rust_dc,
                files: &["package.json"],
                extensions: &["js"],
                folders: &["node_modules"],
            }
            .is_match()
        );
        rust.close()?;

        let java = testdir(&["README.md", "src/com/test/Main.java", "pom.xml"])?;
        let java_dc = DirContents::from_path(java.path(), follow_symlinks)?;
        assert!(
            !ScanDir {
                dir_contents: &java_dc,
                files: &["package.json"],
                extensions: &["js"],
                folders: &["node_modules"],
            }
            .is_match()
        );
        java.close()?;

        let node = testdir(&["README.md", "node_modules/lodash/main.js", "package.json"])?;
        let node_dc = DirContents::from_path(node.path(), follow_symlinks)?;
        assert!(
            ScanDir {
                dir_contents: &node_dc,
                files: &["package.json"],
                extensions: &["js"],
                folders: &["node_modules"],
            }
            .is_match()
        );
        node.close()?;

        let tarballs = testdir(&["foo.tgz", "foo.tar.gz"])?;
        let tarballs_dc = DirContents::from_path(tarballs.path(), follow_symlinks)?;
        assert!(
            ScanDir {
                dir_contents: &tarballs_dc,
                files: &[],
                extensions: &["tar.gz"],
                folders: &[],
            }
            .is_match()
        );
        tarballs.close()?;

        let dont_match_ext = testdir(&["foo.js", "foo.ts"])?;
        let dont_match_ext_dc = DirContents::from_path(dont_match_ext.path(), follow_symlinks)?;
        assert!(
            !ScanDir {
                dir_contents: &dont_match_ext_dc,
                files: &[],
                extensions: &["js", "!notfound", "!ts"],
                folders: &[],
            }
            .is_match()
        );
        dont_match_ext.close()?;

        let dont_match_file = testdir(&["goodfile", "evilfile"])?;
        let dont_match_file_dc = DirContents::from_path(dont_match_file.path(), follow_symlinks)?;
        assert!(
            !ScanDir {
                dir_contents: &dont_match_file_dc,
                files: &["goodfile", "!notfound", "!evilfile"],
                extensions: &[],
                folders: &[],
            }
            .is_match()
        );
        dont_match_file.close()?;

        let dont_match_folder = testdir(&["gooddir/somefile", "evildir/somefile"])?;
        let dont_match_folder_dc =
            DirContents::from_path(dont_match_folder.path(), follow_symlinks)?;
        assert!(
            !ScanDir {
                dir_contents: &dont_match_folder_dc,
                files: &[],
                extensions: &[],
                folders: &["gooddir", "!notfound", "!evildir"],
            }
            .is_match()
        );
        dont_match_folder.close()?;

        Ok(())
    }

    #[test]
    fn context_constructor_should_canonicalize_current_dir() -> io::Result<()> {
        #[cfg(not(windows))]
        use std::os::unix::fs::symlink as symlink_dir;
        #[cfg(windows)]
        use std::os::windows::fs::symlink_dir;

        let tmp_dir = tempfile::TempDir::new()?;
        let path = tmp_dir.path().join("a/xxx/yyy");
        fs::create_dir_all(path)?;

        // Set up a mock symlink
        let path_actual = tmp_dir.path().join("a/xxx");
        let path_symlink = tmp_dir.path().join("a/symlink");
        symlink_dir(&path_actual, &path_symlink).expect("create symlink");

        // Mock navigation into the symlink path
        let test_path = path_symlink.join("yyy");
        let context = Context::new_with_shell_and_path(
            Default::default(),
            Shell::Unknown,
            Target::Main,
            test_path.clone(),
            test_path.clone(),
            Default::default(),
        );

        assert_ne!(context.current_dir, context.logical_dir);

        let expected_current_dir =
            dunce::canonicalize(path_actual.join("yyy")).expect("canonicalize");
        assert_eq!(expected_current_dir, context.current_dir);

        let expected_logical_dir = test_path;
        assert_eq!(expected_logical_dir, context.logical_dir);

        tmp_dir.close()
    }

    #[test]
    fn context_constructor_should_fail_gracefully_when_canonicalization_fails() {
        // Mock navigation to a directory which does not exist on disk
        let test_path = Path::new("/path_which_does_not_exist").to_path_buf();
        let context = Context::new_with_shell_and_path(
            Default::default(),
            Shell::Unknown,
            Target::Main,
            test_path.clone(),
            test_path.clone(),
            Default::default(),
        );

        let expected_current_dir = &test_path;
        assert_eq!(expected_current_dir, &context.current_dir);

        let expected_logical_dir = &test_path;
        assert_eq!(expected_logical_dir, &context.logical_dir);
    }

    #[test]
    fn context_constructor_should_fall_back_to_tilde_replacement_when_canonicalization_fails() {
        use utils::home_dir;

        // Mock navigation to a directory which does not exist on disk
        let test_path = Path::new("~/path_which_does_not_exist").to_path_buf();
        let context = Context::new_with_shell_and_path(
            Default::default(),
            Shell::Unknown,
            Target::Main,
            test_path.clone(),
            test_path.clone(),
            Default::default(),
        );

        let expected_current_dir = home_dir()
            .expect("home_dir")
            .join("path_which_does_not_exist");
        assert_eq!(expected_current_dir, context.current_dir);

        let expected_logical_dir = test_path;
        assert_eq!(expected_logical_dir, context.logical_dir);
    }

    #[test]
    fn set_config_method_overwrites_constructor() {
        let context = default_context();
        let mod_context = default_context().set_config(toml::toml! {
            add_newline = true
        });

        assert_ne!(context.config.config, mod_context.config.config);
    }

    #[cfg(windows)]
    #[test]
    fn strip_extended_path_prefix() {
        let test_path = Path::new(r"\\?\C:\").to_path_buf();
        let context = Context::new_with_shell_and_path(
            Properties::default(),
            Shell::Unknown,
            Target::Main,
            test_path.clone(),
            test_path,
            Default::default(),
        );

        let expected_path = Path::new(r"C:\");

        assert_eq!(&context.current_dir, expected_path);
    }
}
