use process_control::{ChildExt, Control};
use std::ffi::OsStr;
use std::fmt::Debug;
use std::fs::read_to_string;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::context::Context;
use crate::context::Shell;

/// Create a `PathBuf` from an absolute path, where the root directory will be mocked in test
#[cfg(not(test))]
#[inline]
#[allow(dead_code)]
pub fn context_path<S: AsRef<OsStr> + ?Sized>(_context: &Context, s: &S) -> PathBuf {
    PathBuf::from(s)
}

/// Create a `PathBuf` from an absolute path, where the root directory will be mocked in test
#[cfg(test)]
#[allow(dead_code)]
pub fn context_path<S: AsRef<OsStr> + ?Sized>(context: &Context, s: &S) -> PathBuf {
    let requested_path = PathBuf::from(s);

    if requested_path.is_absolute() {
        let mut path = PathBuf::from(context.root_dir.path());
        path.extend(requested_path.components().skip(1));
        path
    } else {
        requested_path
    }
}

/// Return the string contents of a file
pub fn read_file<P: AsRef<Path> + Debug>(file_name: P) -> Result<String> {
    log::trace!("Trying to read from {file_name:?}");

    let result = read_to_string(file_name);

    if result.is_err() {
        log::debug!("Error reading file: {result:?}");
    } else {
        log::trace!("File read successfully");
    };

    result
}

/// Write a string to a file
#[cfg(test)]
pub fn write_file<P: AsRef<Path>, S: AsRef<str>>(file_name: P, text: S) -> Result<()> {
    use std::io::Write;

    let file_name = file_name.as_ref();
    let text = text.as_ref();

    log::trace!("Trying to write {text:?} to {file_name:?}");
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(file_name)
    {
        Ok(file) => file,
        Err(err) => {
            log::warn!("Error creating file: {err:?}");
            return Err(err);
        }
    };

    match file.write_all(text.as_bytes()) {
        Ok(()) => {
            log::trace!("File {file_name:?} written successfully");
        }
        Err(err) => {
            log::warn!("Error writing to file: {err:?}");
            return Err(err);
        }
    }
    file.sync_all()
}

/// Reads command output from stderr or stdout depending on to which stream program streamed it's output
pub fn get_command_string_output(command: CommandOutput) -> String {
    if command.stdout.is_empty() {
        command.stderr
    } else {
        command.stdout
    }
}

/// Attempt to resolve `binary_name` from and creates a new `Command` pointing at it
/// This allows executing cmd files on Windows and prevents running executable from cwd on Windows
/// This function also initializes std{err,out,in} to protect against processes changing the console mode
pub fn create_command<T: AsRef<OsStr>>(binary_name: T) -> Result<Command> {
    let binary_name = binary_name.as_ref();
    log::trace!("Creating Command for binary {binary_name:?}");

    let full_path = match which::which(binary_name) {
        Ok(full_path) => {
            log::trace!("Using {full_path:?} as {binary_name:?}");
            full_path
        }
        Err(error) => {
            log::trace!("Unable to find {binary_name:?} in PATH, {error:?}");
            return Err(Error::new(ErrorKind::NotFound, error));
        }
    };

    #[allow(clippy::disallowed_methods)]
    let mut cmd = Command::new(full_path);
    cmd.stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .stdin(Stdio::null());

    Ok(cmd)
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

impl PartialEq for CommandOutput {
    fn eq(&self, other: &Self) -> bool {
        self.stdout == other.stdout && self.stderr == other.stderr
    }
}

#[cfg(test)]
pub fn display_command<T: AsRef<OsStr> + Debug, U: AsRef<OsStr> + Debug>(
    cmd: T,
    args: &[U],
) -> String {
    std::iter::once(cmd.as_ref())
        .chain(args.iter().map(AsRef::as_ref))
        .map(|i| i.to_string_lossy().into_owned())
        .collect::<Vec<String>>()
        .join(" ")
}

/// Execute a command and return the output on stdout and stderr if successful
pub fn exec_cmd<T: AsRef<OsStr> + Debug, U: AsRef<OsStr> + Debug>(
    cmd: T,
    args: &[U],
    time_limit: Duration,
) -> Option<CommandOutput> {
    log::trace!("Executing command {cmd:?} with args {args:?}");
    #[cfg(test)]
    if let Some(o) = mock_cmd(&cmd, args) {
        return o;
    }
    internal_exec_cmd(cmd, args, time_limit)
}

#[cfg(test)]
pub fn mock_cmd<T: AsRef<OsStr> + Debug, U: AsRef<OsStr> + Debug>(
    cmd: T,
    args: &[U],
) -> Option<Option<CommandOutput>> {
    let command = display_command(&cmd, args);
    let out = match command.as_str() {
        "bun --version" => Some(CommandOutput {
            stdout: String::from("0.1.4\n"),
            stderr: String::default(),
        }),
        "buf --version" => Some(CommandOutput {
            stdout: String::from("1.0.0"),
            stderr: String::default(),
        }),
        "cc --version" => Some(CommandOutput {
            stdout: String::from(
                "\
FreeBSD clang version 11.0.1 (git@github.com:llvm/llvm-project.git llvmorg-11.0.1-0-g43ff75f2c3fe)
Target: x86_64-unknown-freebsd13.0
Thread model: posix
InstalledDir: /usr/bin",
            ),
            stderr: String::default(),
        }),
        "gcc --version" => Some(CommandOutput {
            stdout: String::from(
                "\
cc (Debian 10.2.1-6) 10.2.1 20210110
Copyright (C) 2020 Free Software Foundation, Inc.
This is free software; see the source for copying conditions.  There is NO
warranty; not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.",
            ),
            stderr: String::default(),
        }),
        "clang --version" => Some(CommandOutput {
            stdout: String::from(
                "\
OpenBSD clang version 11.1.0
Target: amd64-unknown-openbsd7.0
Thread model: posix
InstalledDir: /usr/bin",
            ),
            stderr: String::default(),
        }),
        "c++ --version" => Some(CommandOutput {
            stdout: String::from(
                "\
c++ (GCC) 14.2.1 20240910
Copyright (C) 2024 Free Software Foundation, Inc.
This is free software; see the source for copying conditions.  There is NO
warranty; not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.",
            ),
            stderr: String::default(),
        }),
        "g++ --version" => Some(CommandOutput {
            stdout: String::from(
                "\
g++ (GCC) 14.2.1 20240910
Copyright (C) 2024 Free Software Foundation, Inc.
This is free software; see the source for copying conditions.  There is NO
warranty; not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.",
            ),
            stderr: String::default(),
        }),
        "clang++ --version" => Some(CommandOutput {
            stdout: String::from(
                "\
clang version 19.1.7
Target: x86_64-pc-linux-gnu
Thread model: posix
InstalledDir: /usr/bin",
            ),
            stderr: String::default(),
        }),
        "cobc -version" => Some(CommandOutput {
            stdout: String::from(
                "\
cobc (GnuCOBOL) 3.1.2.0
Copyright (C) 2020 Free Software Foundation, Inc.
License GPLv3+: GNU GPL version 3 or later <https://gnu.org/licenses/gpl.html>
This is free software; see the source for copying conditions.  There is NO
warranty; not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
Written by Keisuke Nishida, Roger While, Ron Norman, Simon Sobisch, Edward Hart
Built     Dec 24 2020 19:08:58
Packaged  Dec 23 2020 12:04:58 UTC
C version \"10.2.0\"",
            ),
            stderr: String::default(),
        }),
        "crystal --version" => Some(CommandOutput {
            stdout: String::from(
                "\
Crystal 0.35.1 (2020-06-19)

LLVM: 10.0.0
Default target: x86_64-apple-macosx\n",
            ),
            stderr: String::default(),
        }),
        "dart --version" => Some(CommandOutput {
            stdout: String::default(),
            stderr: String::from(
                "Dart VM version: 2.8.4 (stable) (Wed Jun 3 12:26:04 2020 +0200) on \"macos_x64\"",
            ),
        }),
        "deno -V" => Some(CommandOutput {
            stdout: String::from("deno 1.8.3\n"),
            stderr: String::default(),
        }),
        "dummy_command" => Some(CommandOutput {
            stdout: String::from("stdout ok!\n"),
            stderr: String::from("stderr ok!\n"),
        }),
        "elixir --version" => Some(CommandOutput {
            stdout: String::from(
                "\
Erlang/OTP 22 [erts-10.6.4] [source] [64-bit] [smp:8:8] [ds:8:8:10] [async-threads:1] [hipe]

Elixir 1.10 (compiled with Erlang/OTP 22)\n",
            ),
            stderr: String::default(),
        }),
        "elm --version" => Some(CommandOutput {
            stdout: String::from("0.19.1\n"),
            stderr: String::default(),
        }),
        "fennel --version" => Some(CommandOutput {
            stdout: String::from("Fennel 1.2.1 on PUC Lua 5.4\n"),
            stderr: String::default(),
        }),
        "fossil branch current" => Some(CommandOutput {
            stdout: String::from("topic-branch"),
            stderr: String::default(),
        }),
        "fossil branch new topic-branch trunk" => Some(CommandOutput {
            stdout: String::default(),
            stderr: String::default(),
        }),
        "fossil diff -i --numstat" => Some(CommandOutput {
            stdout: String::from(
                "\
         3          2 README.md
         3          2 TOTAL over 1 changed files",
            ),
            stderr: String::default(),
        }),
        "fossil update topic-branch" => Some(CommandOutput {
            stdout: String::default(),
            stderr: String::default(),
        }),
        "gleam --version" => Some(CommandOutput {
            stdout: String::from("gleam 1.0.0\n"),
            stderr: String::default(),
        }),
        "go version" => Some(CommandOutput {
            stdout: String::from("go version go1.12.1 linux/amd64\n"),
            stderr: String::default(),
        }),
        "ghc --numeric-version" => Some(CommandOutput {
            stdout: String::from("9.2.1\n"),
            stderr: String::default(),
        }),
        "helm version --short --client" => Some(CommandOutput {
            stdout: String::from("v3.1.1+gafe7058\n"),
            stderr: String::default(),
        }),
        s if s.ends_with("java -Xinternalversion") => Some(CommandOutput {
            stdout: String::from(
                "OpenJDK 64-Bit Server VM (13.0.2+8) for bsd-amd64 JRE (13.0.2+8), built on Feb  6 2020 02:07:52 by \"brew\" with clang 4.2.1 Compatible Apple LLVM 11.0.0 (clang-1100.0.33.17)",
            ),
            stderr: String::default(),
        }),
        "scala-cli version --scala" => Some(CommandOutput {
            stdout: String::from("3.4.1"),
            stderr: String::default(),
        }),
        "scalac -version" => Some(CommandOutput {
            stdout: String::from(
                "Scala compiler version 2.13.5 -- Copyright 2002-2020, LAMP/EPFL and Lightbend, Inc.",
            ),
            stderr: String::default(),
        }),
        "julia --version" => Some(CommandOutput {
            stdout: String::from("julia version 1.4.0\n"),
            stderr: String::default(),
        }),
        "kotlin -version" => Some(CommandOutput {
            stdout: String::from("Kotlin version 1.4.21-release-411 (JRE 14.0.1+7)\n"),
            stderr: String::default(),
        }),
        "kotlinc -version" => Some(CommandOutput {
            stdout: String::from("info: kotlinc-jvm 1.4.21 (JRE 14.0.1+7)\n"),
            stderr: String::default(),
        }),
        "lua -v" => Some(CommandOutput {
            stdout: String::from("Lua 5.4.0  Copyright (C) 1994-2020 Lua.org, PUC-Rio\n"),
            stderr: String::default(),
        }),
        "luajit -v" => Some(CommandOutput {
            stdout: String::from(
                "LuaJIT 2.0.5 -- Copyright (C) 2005-2017 Mike Pall. http://luajit.org/\n",
            ),
            stderr: String::default(),
        }),
        "mojo --version" => Some(CommandOutput {
            stdout: String::from("mojo 24.4.0 (2cb57382)\n"),
            stderr: String::default(),
        }),
        "nats context info --json" => Some(CommandOutput {
            stdout: String::from("{\"name\":\"localhost\",\"url\":\"nats://localhost:4222\"}"),
            stderr: String::default(),
        }),
        "nim --version" => Some(CommandOutput {
            stdout: String::from(
                "\
Nim Compiler Version 1.2.0 [Linux: amd64]
Compiled at 2020-04-03
Copyright (c) 2006-2020 by Andreas Rumpf
git hash: 7e83adff84be5d0c401a213eccb61e321a3fb1ff
active boot switches: -d:release\n",
            ),
            stderr: String::default(),
        }),
        "node --version" => Some(CommandOutput {
            stdout: String::from("v12.0.0\n"),
            stderr: String::default(),
        }),
        "ocaml -vnum" => Some(CommandOutput {
            stdout: String::from("4.10.0\n"),
            stderr: String::default(),
        }),
        "odin version" => Some(CommandOutput {
            stdout: String::from("odin version dev-2024-03:fc587c507\n"),
            stderr: String::default(),
        }),
        "opa version" => Some(CommandOutput {
            stdout: String::from(
                "Version: 0.44.0
Build Commit: e8d488f
Build Timestamp: 2022-09-07T23:50:25Z
Build Hostname: 119428673f4c
Go Version: go1.19.1
Platform: linux/amd64
WebAssembly: unavailable
",
            ),
            stderr: String::default(),
        }),
        "opam switch show --safe" => Some(CommandOutput {
            stdout: String::from("default\n"),
            stderr: String::default(),
        }),
        "typst --version" => Some(CommandOutput {
            stdout: String::from("typst 0.10 (360cc9b9)"),
            stderr: String::default(),
        }),

        "esy ocaml -vnum" => Some(CommandOutput {
            stdout: String::from("4.08.1\n"),
            stderr: String::default(),
        }),
        "perl -e printf q#%vd#,$^V;" => Some(CommandOutput {
            stdout: String::from("5.26.1"),
            stderr: String::default(),
        }),
        "php -nr echo PHP_MAJOR_VERSION.\".\".PHP_MINOR_VERSION.\".\".PHP_RELEASE_VERSION;" => {
            Some(CommandOutput {
                stdout: String::from("7.3.8"),
                stderr: String::default(),
            })
        }
        "pijul channel" => Some(CommandOutput {
            stdout: String::from("  main\n* tributary-48198"),
            stderr: String::default(),
        }),
        "pijul channel new tributary-48198" => Some(CommandOutput {
            stdout: String::default(),
            stderr: String::default(),
        }),
        "pijul channel switch tributary-48198" => Some(CommandOutput {
            stdout: String::from("Outputting repository ↖"),
            stderr: String::default(),
        }),
        "pixi --version" => Some(CommandOutput {
            stdout: String::from("pixi 0.33.0"),
            stderr: String::default(),
        }),
        "pulumi version" => Some(CommandOutput {
            stdout: String::from("1.2.3-ver.1631311768+e696fb6c"),
            stderr: String::default(),
        }),
        "purs --version" => Some(CommandOutput {
            stdout: String::from("0.13.5\n"),
            stderr: String::default(),
        }),
        "pyenv version-name" => Some(CommandOutput {
            stdout: String::from("system\n"),
            stderr: String::default(),
        }),
        "python --version" => None,
        "python2 --version" => Some(CommandOutput {
            stdout: String::default(),
            stderr: String::from("Python 2.7.17\n"),
        }),
        "python3 --version" => Some(CommandOutput {
            stdout: String::from("Python 3.8.0\n"),
            stderr: String::default(),
        }),
        "quarto --version" => Some(CommandOutput {
            stdout: String::from("1.4.549\n"),
            stderr: String::default(),
        }),
        "R --version" => Some(CommandOutput {
            stdout: String::default(),
            stderr: String::from(
                r#"R version 4.1.0 (2021-05-18) -- "Camp Pontanezen"
Copyright (C) 2021 The R Foundation for Statistical Computing
Platform: x86_64-w64-mingw32/x64 (64-bit)\n

R is free software and comes with ABSOLUTELY NO WARRANTY.
You are welcome to redistribute it under the terms of the
GNU General Public License versions 2 or 3.
For more information about these matters see
https://www.gnu.org/licenses/."#,
            ),
        }),
        "raku --version" => Some(CommandOutput {
            stdout: String::from(
                "\
Welcome to Rakudo™ v2021.12.
Implementing the Raku® Programming Language v6.d.
Built on MoarVM version 2021.12.\n",
            ),
            stderr: String::default(),
        }),
        "red --version" => Some(CommandOutput {
            stdout: String::from("0.6.4\n"),
            stderr: String::default(),
        }),
        "ruby -v" => Some(CommandOutput {
            stdout: String::from("ruby 2.5.1p57 (2018-03-29 revision 63029) [x86_64-linux-gnu]\n"),
            stderr: String::default(),
        }),
        "solc --version" => Some(CommandOutput {
            stdout: String::from(
                "solc, the solidity compiler commandline interface
Version: 0.8.16+commit.07a7930e.Linux.g++",
            ),
            stderr: String::default(),
        }),
        "solcjs --version" => Some(CommandOutput {
            stdout: String::from("0.8.15+commit.e14f2714.Emscripten.clang"),
            stderr: String::default(),
        }),
        "swift --version" => Some(CommandOutput {
            stdout: String::from(
                "\
Apple Swift version 5.2.2 (swiftlang-1103.0.32.6 clang-1103.0.32.51)
Target: x86_64-apple-darwin19.4.0\n",
            ),
            stderr: String::default(),
        }),
        "vagrant --version" => Some(CommandOutput {
            stdout: String::from("Vagrant 2.2.10\n"),
            stderr: String::default(),
        }),
        "v version" => Some(CommandOutput {
            stdout: String::from("V 0.2 30c0659"),
            stderr: String::default(),
        }),
        "xmake --version" => Some(CommandOutput {
            stdout: String::from(
                r"xmake v2.9.5+HEAD.0db4fe6, A cross-platform build utility based on Lua
Copyright (C) 2015-present Ruki Wang, tboox.org, xmake.io
                         _
    __  ___ __  __  __ _| | ______
    \ \/ / |  \/  |/ _  | |/ / __ \
     >  <  | \__/ | /_| |   <  ___/
    /_/\_\_|_|  |_|\__ \|_|\_\____|
                         by ruki, xmake.io
    👉  Manual: https://xmake.io/#/getting_started
    🙏  Donate: https://xmake.io/#/sponsor",
            ),
            stderr: String::default(),
        }),
        "zig version" => Some(CommandOutput {
            stdout: String::from("0.6.0\n"),
            stderr: String::default(),
        }),
        "cmake --version" => Some(CommandOutput {
            stdout: String::from(
                "\
cmake version 3.17.3

CMake suite maintained and supported by Kitware (kitware.com/cmake).\n",
            ),
            stderr: String::default(),
        }),
        "dotnet --version" => Some(CommandOutput {
            stdout: String::from("3.1.103"),
            stderr: String::default(),
        }),
        "dotnet --list-sdks" => Some(CommandOutput {
            stdout: String::from("3.1.103 [/usr/share/dotnet/sdk]"),
            stderr: String::default(),
        }),
        "terraform version" => Some(CommandOutput {
            stdout: String::from("Terraform v0.12.14\n"),
            stderr: String::default(),
        }),
        s if s.starts_with("erl -noshell -eval") => Some(CommandOutput {
            stdout: String::from("22.1.3\n"),
            stderr: String::default(),
        }),
        _ => return None,
    };
    Some(out)
}

/// Wraps ANSI color escape sequences in the shell-appropriate wrappers.
pub fn wrap_colorseq_for_shell(ansi: String, shell: Shell) -> String {
    const ESCAPE_BEGIN: char = '\u{1b}';
    const ESCAPE_END: char = 'm';
    wrap_seq_for_shell(ansi, shell, ESCAPE_BEGIN, ESCAPE_END)
}

/// Many shells cannot deal with raw unprintable characters and miscompute the cursor position,
/// leading to strange visual bugs like duplicated/missing chars. This function wraps a specified
/// sequence in shell-specific escapes to avoid these problems.
pub fn wrap_seq_for_shell(
    ansi: String,
    shell: Shell,
    escape_begin: char,
    escape_end: char,
) -> String {
    let (beg, end) = match shell {
        // \[ and \]
        Shell::Bash => ("\u{5c}\u{5b}", "\u{5c}\u{5d}"),
        // %{ and %}
        Shell::Tcsh | Shell::Zsh => ("\u{25}\u{7b}", "\u{25}\u{7d}"),
        _ => return ansi,
    };

    // ANSI escape codes cannot be nested, so we can keep track of whether we're
    // in an escape or not with a single boolean variable
    let mut escaped = false;
    let final_string: String = ansi
        .chars()
        .map(|x| {
            if x == escape_begin && !escaped {
                escaped = true;
                format!("{beg}{escape_begin}")
            } else if x == escape_end && escaped {
                escaped = false;
                format!("{escape_end}{end}")
            } else {
                x.to_string()
            }
        })
        .collect();
    final_string
}

fn internal_exec_cmd<T: AsRef<OsStr> + Debug, U: AsRef<OsStr> + Debug>(
    cmd: T,
    args: &[U],
    time_limit: Duration,
) -> Option<CommandOutput> {
    let mut cmd = create_command(cmd).ok()?;
    cmd.args(args);
    exec_timeout(&mut cmd, time_limit)
}

pub fn exec_timeout(cmd: &mut Command, time_limit: Duration) -> Option<CommandOutput> {
    let start = Instant::now();
    let process = match cmd.spawn() {
        Ok(process) => process,
        Err(error) => {
            log::info!("Unable to run {:?}, {:?}", cmd.get_program(), error);
            return None;
        }
    };
    match process
        .controlled_with_output()
        .time_limit(time_limit)
        .terminate_for_timeout()
        .wait()
    {
        Ok(Some(output)) => {
            let stdout_string = match String::from_utf8(output.stdout) {
                Ok(stdout) => stdout,
                Err(error) => {
                    log::warn!("Unable to decode stdout: {error:?}");
                    return None;
                }
            };
            let stderr_string = match String::from_utf8(output.stderr) {
                Ok(stderr) => stderr,
                Err(error) => {
                    log::warn!("Unable to decode stderr: {error:?}");
                    return None;
                }
            };

            log::trace!(
                "stdout: {:?}, stderr: {:?}, exit code: \"{:?}\", took {:?}",
                stdout_string,
                stderr_string,
                output.status.code(),
                start.elapsed()
            );

            if !output.status.success() {
                return None;
            }

            Some(CommandOutput {
                stdout: stdout_string,
                stderr: stderr_string,
            })
        }
        Ok(None) => {
            log::warn!("Executing command {:?} timed out.", cmd.get_program());
            log::warn!(
                "You can set command_timeout in your config to a higher value to allow longer-running commands to keep executing."
            );
            None
        }
        Err(error) => {
            log::info!(
                "Executing command {:?} failed by: {:?}",
                cmd.get_program(),
                error
            );
            None
        }
    }
}

// Render the time into a nice human-readable string
pub fn render_time(raw_millis: u128, show_millis: bool) -> String {
    // Fast returns for zero cases to render something
    match (raw_millis, show_millis) {
        (0, true) => return "0ms".into(),
        (0..=999, false) => return "0s".into(),
        _ => (),
    }

    // Calculate a simple breakdown into days/hours/minutes/seconds/milliseconds
    let (millis, raw_seconds) = (raw_millis % 1000, raw_millis / 1000);
    let (seconds, raw_minutes) = (raw_seconds % 60, raw_seconds / 60);
    let (minutes, raw_hours) = (raw_minutes % 60, raw_minutes / 60);
    let (hours, days) = (raw_hours % 24, raw_hours / 24);

    // Calculate how long the string will be to allocate once in most cases
    let result_capacity = match raw_millis {
        1..=59 => 3,
        60..=3599 => 6,
        3600..=86399 => 9,
        _ => 12,
    } + if show_millis { 5 } else { 0 };

    let components = [(days, "d"), (hours, "h"), (minutes, "m"), (seconds, "s")];

    // Concat components ito result starting from the first non-zero one
    let result = components.iter().fold(
        String::with_capacity(result_capacity),
        |acc, (component, suffix)| match component {
            0 if acc.is_empty() => acc,
            n => acc + &n.to_string() + suffix,
        },
    );

    if show_millis {
        result + &millis.to_string() + "ms"
    } else {
        result
    }
}

pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

const HEXTABLE: &[char] = &[
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// Encode a u8 slice into a hexadecimal string.
pub fn encode_to_hex(slice: &[u8]) -> String {
    // let mut j = 0;
    let mut dst = Vec::with_capacity(slice.len() * 2);
    for &v in slice {
        dst.push(HEXTABLE[(v >> 4) as usize] as u8);
        dst.push(HEXTABLE[(v & 0x0f) as usize] as u8);
    }
    String::from_utf8(dst).unwrap()
}

pub trait PathExt {
    /// Get device / volume info
    fn device_id(&self) -> Option<u64>;
}

#[cfg(windows)]
impl PathExt for Path {
    fn device_id(&self) -> Option<u64> {
        // Maybe it should use unimplemented!
        Some(42u64)
    }
}

#[cfg(not(windows))]
impl PathExt for Path {
    #[cfg(target_os = "linux")]
    fn device_id(&self) -> Option<u64> {
        use std::os::linux::fs::MetadataExt;
        match self.metadata() {
            Ok(m) => Some(m.st_dev()),
            Err(_) => None,
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn device_id(&self) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        match self.metadata() {
            Ok(m) => Some(m.dev()),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_time_test_0ms() {
        assert_eq!(render_time(0_u128, true), "0ms")
    }
    #[test]
    fn render_time_test_0s() {
        assert_eq!(render_time(0_u128, false), "0s")
    }
    #[test]
    fn render_time_test_500ms() {
        assert_eq!(render_time(500_u128, true), "500ms")
    }
    #[test]
    fn render_time_test_500ms_no_millis() {
        assert_eq!(render_time(500_u128, false), "0s")
    }
    #[test]
    fn render_time_test_10s() {
        assert_eq!(render_time(10_000_u128, true), "10s0ms")
    }
    #[test]
    fn render_time_test_90s() {
        assert_eq!(render_time(90_000_u128, true), "1m30s0ms")
    }
    #[test]
    fn render_time_test_10110s() {
        assert_eq!(render_time(10_110_000_u128, true), "2h48m30s0ms")
    }
    #[test]
    fn render_time_test_1d() {
        assert_eq!(render_time(86_400_000_u128, false), "1d0h0m0s")
    }

    #[test]
    fn exec_mocked_command() {
        let result = exec_cmd(
            "dummy_command",
            &[] as &[&OsStr],
            Duration::from_millis(500),
        );
        let expected = Some(CommandOutput {
            stdout: String::from("stdout ok!\n"),
            stderr: String::from("stderr ok!\n"),
        });

        assert_eq!(result, expected)
    }

    // While the exec_cmd should work on Windows some of these tests assume a Unix-like
    // environment.

    #[test]
    #[cfg(not(windows))]
    fn exec_no_output() {
        let result = internal_exec_cmd("true", &[] as &[&OsStr], Duration::from_millis(500));
        let expected = Some(CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
        });

        assert_eq!(result, expected)
    }

    #[test]
    #[cfg(not(windows))]
    fn exec_with_output_stdout() {
        let result =
            internal_exec_cmd("/bin/sh", &["-c", "echo hello"], Duration::from_millis(500));
        let expected = Some(CommandOutput {
            stdout: String::from("hello\n"),
            stderr: String::new(),
        });

        assert_eq!(result, expected)
    }

    #[test]
    #[cfg(not(windows))]
    fn exec_with_output_stderr() {
        let result = internal_exec_cmd(
            "/bin/sh",
            &["-c", "echo hello >&2"],
            Duration::from_millis(500),
        );
        let expected = Some(CommandOutput {
            stdout: String::new(),
            stderr: String::from("hello\n"),
        });

        assert_eq!(result, expected)
    }

    #[test]
    #[cfg(not(windows))]
    fn exec_with_output_both() {
        let result = internal_exec_cmd(
            "/bin/sh",
            &["-c", "echo hello; echo world >&2"],
            Duration::from_millis(500),
        );
        let expected = Some(CommandOutput {
            stdout: String::from("hello\n"),
            stderr: String::from("world\n"),
        });

        assert_eq!(result, expected)
    }

    #[test]
    #[cfg(not(windows))]
    fn exec_with_non_zero_exit_code() {
        let result = internal_exec_cmd("false", &[] as &[&OsStr], Duration::from_millis(500));
        let expected = None;

        assert_eq!(result, expected)
    }

    #[test]
    #[cfg(not(windows))]
    fn exec_slow_command() {
        let result = internal_exec_cmd("sleep", &["500"], Duration::from_millis(500));
        let expected = None;

        assert_eq!(result, expected)
    }

    #[test]
    fn test_color_sequence_wrappers() {
        let test0 = "\x1b2mhellomynamekeyes\x1b2m"; // BEGIN: \x1b     END: m
        let test1 = "\x1b]330;mlol\x1b]0m"; // BEGIN: \x1b     END: m
        let test2 = "\u{1b}J"; // BEGIN: \x1b     END: J
        let test3 = "OH NO"; // BEGIN: O    END: O
        let test4 = "herpaderp";
        let test5 = "";

        let zresult0 = wrap_seq_for_shell(test0.to_string(), Shell::Zsh, '\x1b', 'm');
        let zresult1 = wrap_seq_for_shell(test1.to_string(), Shell::Zsh, '\x1b', 'm');
        let zresult2 = wrap_seq_for_shell(test2.to_string(), Shell::Zsh, '\x1b', 'J');
        let zresult3 = wrap_seq_for_shell(test3.to_string(), Shell::Zsh, 'O', 'O');
        let zresult4 = wrap_seq_for_shell(test4.to_string(), Shell::Zsh, '\x1b', 'm');
        let zresult5 = wrap_seq_for_shell(test5.to_string(), Shell::Zsh, '\x1b', 'm');

        assert_eq!(&zresult0, "%{\x1b2m%}hellomynamekeyes%{\x1b2m%}");
        assert_eq!(&zresult1, "%{\x1b]330;m%}lol%{\x1b]0m%}");
        assert_eq!(&zresult2, "%{\x1bJ%}");
        assert_eq!(&zresult3, "%{OH NO%}");
        assert_eq!(&zresult4, "herpaderp");
        assert_eq!(&zresult5, "");

        let bresult0 = wrap_seq_for_shell(test0.to_string(), Shell::Bash, '\x1b', 'm');
        let bresult1 = wrap_seq_for_shell(test1.to_string(), Shell::Bash, '\x1b', 'm');
        let bresult2 = wrap_seq_for_shell(test2.to_string(), Shell::Bash, '\x1b', 'J');
        let bresult3 = wrap_seq_for_shell(test3.to_string(), Shell::Bash, 'O', 'O');
        let bresult4 = wrap_seq_for_shell(test4.to_string(), Shell::Bash, '\x1b', 'm');
        let bresult5 = wrap_seq_for_shell(test5.to_string(), Shell::Bash, '\x1b', 'm');

        assert_eq!(&bresult0, "\\[\x1b2m\\]hellomynamekeyes\\[\x1b2m\\]");
        assert_eq!(&bresult1, "\\[\x1b]330;m\\]lol\\[\x1b]0m\\]");
        assert_eq!(&bresult2, "\\[\x1bJ\\]");
        assert_eq!(&bresult3, "\\[OH NO\\]");
        assert_eq!(&bresult4, "herpaderp");
        assert_eq!(&bresult5, "");
    }

    #[test]
    fn test_get_command_string_output() {
        let case1 = CommandOutput {
            stdout: String::from("stdout"),
            stderr: String::from("stderr"),
        };
        assert_eq!(get_command_string_output(case1), "stdout");
        let case2 = CommandOutput {
            stdout: String::new(),
            stderr: String::from("stderr"),
        };
        assert_eq!(get_command_string_output(case2), "stderr");
    }

    #[test]
    fn sha1_hex() {
        assert_eq!(
            encode_to_hex(&[8, 13, 9, 189, 129, 94]),
            "080d09bd815e".to_string()
        );
    }
}
