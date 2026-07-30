//! The commands themselves.
//!
//! Every command is an `async fn` that talks to the filesystem the same way any
//! other cell would: capabilities in operations, answers in completions, file
//! bytes in a registered buffer. There is no privileged path from the shell to
//! the volume, so what `ls` can reach is exactly what the root it leased allows.

use molt_core::capability::CapabilityError;
use molt_core::cell::Cell;
use molt_fs::{FsDone, FsError, FsOp, Handle, Kind, Name};

use crate::ShellError;
use crate::console::Console;
use crate::session::Session;

/// What an interactive front-end prints before a typed line.
pub const PROMPT: &[u8] = b"molt> ";

const HELP: &[u8] = b"commands:\n\
    \x20 help        this list\n\
    \x20 ls [name]   list root, or a directory inside it\n\
    \x20 cat <name>  print a file\n";

/// A shell holding a session, and nothing else.
pub struct Shell<'ring, 'registry, 'buffer, const R: usize, const N: usize, const M: usize> {
    session: Session<'ring, 'registry, 'buffer, R, N, M>,
}

/// The shell is a cell like any other: it starts from a session, and it comes
/// back from a restart holding no lease, no handle, and no unanswered request.
/// Its supervisor revokes what it opened on its behalf, because a cell that has
/// stopped cannot close anything itself.
impl<'ring, 'registry, 'buffer, const R: usize, const N: usize, const M: usize> Cell
    for Shell<'ring, 'registry, 'buffer, R, N, M>
{
    type Error = ShellError;
    type State = Session<'ring, 'registry, 'buffer, R, N, M>;

    fn spawn(session: Self::State) -> Result<Self, ShellError> {
        Ok(Self { session })
    }

    fn restart(&mut self) -> Result<(), ShellError> {
        self.session.reset();
        Ok(())
    }
}

impl<'ring, 'registry, 'buffer, const R: usize, const N: usize, const M: usize>
    Shell<'ring, 'registry, 'buffer, R, N, M>
{
    /// Runs every line of `text`, echoing each one behind a prompt.
    ///
    /// A canned script is how the shell is exercised until a platform reads
    /// its serial port back; the same lines typed by hand go through
    /// [`Shell::run`] one at a time.
    pub async fn script(&mut self, text: &[u8], out: &mut impl Console) -> Result<(), ShellError> {
        for line in text.split(|&byte| byte == b'\n') {
            let line = trim(line);
            if line.is_empty() {
                continue;
            }
            out.write(PROMPT);
            out.line(line);
            self.run(line, out).await?;
        }
        Ok(())
    }

    /// Runs one command line.
    ///
    /// Something the user can fix — a missing name, a file where a directory
    /// belongs — is printed and forgotten. An error returned from here is the
    /// shell's own: a ring that answered the wrong request, or a volume that
    /// failed a checksum.
    pub async fn run(&mut self, line: &[u8], out: &mut impl Console) -> Result<(), ShellError> {
        let mut words = line.split(u8::is_ascii_whitespace).filter(|word| !word.is_empty());
        let Some(command) = words.next() else {
            return Ok(());
        };
        let argument = words.next();

        match command {
            b"help" => {
                out.write(HELP);
                Ok(())
            }
            b"ls" => match self.ls(argument, out).await {
                Err(error) => self.excuse(b"ls", error, out),
                Ok(()) => Ok(()),
            },
            b"cat" => match argument {
                Some(name) => match self.cat(name, out).await {
                    Err(error) => self.excuse(b"cat", error, out),
                    Ok(()) => Ok(()),
                },
                None => {
                    out.line(b"cat: needs a name");
                    Ok(())
                }
            },
            unknown => {
                out.write(b"unknown command: ");
                out.line(unknown);
                Ok(())
            }
        }
    }

    /// Lists the root, or a directory inside it.
    async fn ls(&mut self, name: Option<&[u8]>, out: &mut impl Console) -> Result<(), ShellError> {
        let opened = match name {
            Some(name) => Some(self.open_entry(name).await?),
            None => None,
        };
        let dir = match opened {
            Some(Handle::Dir(dir)) => dir,
            Some(handle) => {
                self.close(handle).await?;
                out.line(b"ls: not a directory");
                return Ok(());
            }
            None => self.session.root()?,
        };

        let FsDone::Stat(stat) = self.session.request(FsOp::Stat(Handle::Dir(dir))).await? else {
            return Err(ShellError::Protocol);
        };
        for index in 0..stat.entries {
            let FsDone::Entry { name, stat } =
                self.session.request(FsOp::Entry { dir, index }).await?
            else {
                return Err(ShellError::Protocol);
            };
            out.write(name.as_bytes());
            match stat.kind {
                Kind::Dir => out.line(b"/"),
                Kind::File => {
                    out.write(b"  ");
                    out.number(stat.size);
                    out.write(b"\n");
                }
            }
        }

        if opened.is_some() {
            self.close(Handle::Dir(dir)).await?;
        }
        Ok(())
    }

    /// Prints a file, one buffer at a time.
    async fn cat(&mut self, name: &[u8], out: &mut impl Console) -> Result<(), ShellError> {
        let opened = self.open_entry(name).await?;
        let Handle::File(file) = opened else {
            self.close(opened).await?;
            out.line(b"cat: not a file");
            return Ok(());
        };

        let buffer = self.session.target();
        let mut offset = 0;
        let mut last = b'\n';
        loop {
            let FsDone::Read(read) =
                self.session.request(FsOp::Read { file, buffer, offset }).await?
            else {
                return Err(ShellError::Protocol);
            };
            if read == 0 {
                break;
            }
            last = self.session.taken(read, |bytes| {
                out.write(bytes);
                bytes[bytes.len() - 1]
            })?;
            offset += read as u64;
        }
        // A file that does not end in a newline would otherwise leave the next
        // prompt sharing its last line.
        if last != b'\n' {
            out.write(b"\n");
        }
        self.close(Handle::File(file)).await
    }

    async fn open_entry(&mut self, name: &[u8]) -> Result<Handle, ShellError> {
        let name = Name::new(name).map_err(ShellError::Fs)?;
        let dir = self.session.root()?;
        let opened = self.session.request(FsOp::Open { dir, name }).await?;
        opened.handle().ok_or(ShellError::Protocol)
    }

    async fn close(&mut self, handle: Handle) -> Result<(), ShellError> {
        match self.session.request(FsOp::Close(handle)).await? {
            FsDone::Closed => Ok(()),
            _ => Err(ShellError::Protocol),
        }
    }

    /// Reports what the user can fix, and passes on what only the shell can.
    ///
    /// A command that met an ended epoch says so and stops rather than retrying
    /// itself: half a listing is already printed by then, and reprinting it
    /// would be a worse answer than telling the user what happened between the
    /// two halves.
    fn excuse(
        &mut self,
        command: &[u8],
        error: ShellError,
        out: &mut impl Console,
    ) -> Result<(), ShellError> {
        let reason: &[u8] = match error {
            ShellError::Fs(FsError::Missing) => b"no such entry",
            ShellError::Fs(FsError::Name) => b"not a usable name",
            ShellError::Fs(FsError::Kind) => b"wrong kind of object",
            ShellError::Fs(FsError::Cancelled | FsError::Handle(CapabilityError::Stale)) => {
                self.session.release();
                b"storage restarted"
            }
            ShellError::Unavailable => b"no storage",
            other => return Err(other),
        };
        out.write(command);
        out.write(b": ");
        out.line(reason);
        Ok(())
    }
}

fn trim(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(line.len());
    let end = line.iter().rposition(|byte| !byte.is_ascii_whitespace()).map_or(start, |at| at + 1);
    &line[start..end]
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    use std::string::String;
    use std::vec::Vec;

    use molt_block::Loopback;
    use molt_core::buffer::BufferRegistry;
    use molt_core::capability::CellId;
    use molt_core::cell::{Cell, RestartHooks, Supervisor};
    use molt_core::cpu::CpuId;
    use molt_core::registry::Registry;
    use molt_core::ring::IoRing;
    use molt_fs::format::{Tree, build};
    use molt_fs::{Disconnect, Fs, FsDone, FsError, FsOp, Storage, Teardown};

    use super::Shell;
    use crate::capture::Capture;
    use crate::session::Session;
    use crate::{ShellError, drive, drive_bounded};

    const SERVICE: CellId = CellId::new(2);
    const CLIENT: CellId = CellId::new(7);
    /// Smaller than `hello.txt`, so `cat` has to come back for the rest.
    const WINDOW: usize = 8;

    fn image() -> Vec<u8> {
        let mut tree = Tree::new();
        tree.file("hello.txt", b"hello, molt".to_vec()).unwrap();
        tree.file("note.txt", b"one\ntwo\n".to_vec()).unwrap();
        tree.dir("docs").unwrap().file("readme", b"read me\n".to_vec()).unwrap();
        build(&tree, 1).unwrap()
    }

    /// Runs `script` against a fresh volume and returns everything printed.
    fn run(script: &[u8]) -> String {
        let bytes = image();
        let mut scratch = [0u8; WINDOW];
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();

        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
        let mut registry = BufferRegistry::<1>::new();
        let scratch = registry.register_read_write(CLIENT, &mut scratch).unwrap();
        let buffers = RefCell::new(registry);
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let (client, mut driver) = ring.split();

        fs.publish(&mut names.borrow_mut(), SERVICE, CpuId::BOOT).unwrap();
        let session = Session::new(client, &buffers, &names, scratch, WINDOW).unwrap();
        let mut out = Capture::new();
        drive(
            async {
                let mut shell = Shell::spawn(session).unwrap();
                shell.script(script, &mut out).await
            },
            || {
                fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
            },
        )
        .unwrap();
        out.text()
    }

    /// What `run` prints for `line` alone, without prompt or echo.
    fn output(line: &str) -> String {
        let echoed = std::format!("molt> {line}\n");
        run(line.as_bytes()).strip_prefix(&echoed).unwrap().into()
    }

    #[test]
    fn help_lists_commands() {
        let printed = output("help");

        assert!(printed.contains("ls [name]"), "{printed}");
        assert!(printed.contains("cat <name>"), "{printed}");
    }

    #[test]
    fn ls_prints_root() {
        assert_eq!(output("ls"), "docs/\nhello.txt  11\nnote.txt  8\n");
    }

    #[test]
    fn ls_prints_named_directory() {
        assert_eq!(output("ls docs"), "readme  8\n");
    }

    #[test]
    fn ls_refuses_file() {
        assert_eq!(output("ls hello.txt"), "ls: not a directory\n");
    }

    #[test]
    fn cat_prints_file_across_reads() {
        assert_eq!(output("cat hello.txt"), "hello, molt\n");
    }

    #[test]
    fn cat_leaves_trailing_newline_alone() {
        assert_eq!(output("cat note.txt"), "one\ntwo\n");
    }

    #[test]
    fn cat_refuses_directory() {
        assert_eq!(output("cat docs"), "cat: not a file\n");
    }

    #[test]
    fn missing_name_is_reported_not_returned() {
        assert_eq!(output("cat nowhere"), "cat: no such entry\n");
    }

    #[test]
    fn cat_without_name_says_what_it_needs() {
        assert_eq!(output("cat"), "cat: needs a name\n");
    }

    #[test]
    fn unknown_command_names_itself() {
        assert_eq!(output("frobnicate"), "unknown command: frobnicate\n");
    }

    #[test]
    fn script_echoes_lines_behind_prompt() {
        let printed = run(b"  ls  \n\nls docs\n");

        assert_eq!(
            printed,
            "molt> ls\ndocs/\nhello.txt  11\nnote.txt  8\nmolt> ls docs\nreadme  8\n"
        );
    }

    #[test]
    fn shell_with_nothing_published() -> Result<(), ShellError> {
        let mut scratch = [0u8; WINDOW];
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let mut registry = BufferRegistry::<1>::new();
        let scratch = registry.register_read_write(CLIENT, &mut scratch).unwrap();
        let buffers = RefCell::new(registry);
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let (client, _driver) = ring.split();
        let mut shell = Shell::spawn(Session::new(client, &buffers, &names, scratch, WINDOW)?)?;
        let mut out = Capture::new();

        drive(shell.run(b"ls", &mut out), || panic!("a command that asked a service nobody runs"))?;

        assert_eq!(out.text(), "ls: no storage\n");
        Ok(())
    }

    #[test]
    fn withdrawn_mount_reacquired() -> Result<(), ShellError> {
        let bytes = image();
        let mut scratch = [0u8; WINDOW];
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
        let mut registry = BufferRegistry::<1>::new();
        let scratch = registry.register_read_write(CLIENT, &mut scratch).unwrap();
        let buffers = RefCell::new(registry);
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let (client, mut driver) = ring.split();
        fs.publish(&mut names.borrow_mut(), SERVICE, CpuId::BOOT).unwrap();
        let mut shell = Shell::spawn(Session::new(client, &buffers, &names, scratch, WINDOW)?)?;
        let mut out = Capture::new();
        drive(shell.run(b"ls docs", &mut out), || {
            fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
        })?;

        let mut hooks = Teardown::new(&mut driver, &names, SERVICE);
        hooks.stop_submissions();
        hooks.cancel_requests();
        hooks.revoke_capabilities();
        fs.restart().unwrap();

        drive(shell.run(b"ls", &mut out), || panic!("a command that outlived its service"))?;
        fs.publish(&mut names.borrow_mut(), SERVICE, CpuId::BOOT).unwrap();
        drive(shell.run(b"ls docs", &mut out), || {
            fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
        })?;

        assert_eq!(out.text(), "readme  8\nls: no storage\nreadme  8\n");
        Ok(())
    }

    #[test]
    fn abandoned_request_skipped() -> Result<(), ShellError> {
        let bytes = image();
        let mut scratch = [0u8; WINDOW];
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
        let mut registry = BufferRegistry::<1>::new();
        let scratch = registry.register_read_write(CLIENT, &mut scratch).unwrap();
        let buffers = RefCell::new(registry);
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let (client, mut driver) = ring.split();
        fs.publish(&mut names.borrow_mut(), SERVICE, CpuId::BOOT).unwrap();
        let mut shell = Shell::spawn(Session::new(client, &buffers, &names, scratch, WINDOW)?)?;
        let mut out = Capture::new();

        let given_up = drive_bounded(shell.run(b"ls docs", &mut out), 2, || {});

        assert!(given_up.is_none(), "a command finished without anything answering it");
        drive(shell.run(b"ls", &mut out), || {
            fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
        })?;
        assert_eq!(out.text(), "docs/\nhello.txt  11\nnote.txt  8\n");
        Ok(())
    }

    #[test]
    fn shell_misses_deadline_restarted_unasked() -> Result<(), ShellError> {
        const DEADLINE: u64 = 1;
        let bytes = image();
        let mut scratch = [0u8; WINDOW];
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
        let mut registry = BufferRegistry::<1>::new();
        let scratch = registry.register_read_write(CLIENT, &mut scratch).unwrap();
        let buffers = RefCell::new(registry);
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let (client, mut driver) = ring.split();
        fs.publish(&mut names.borrow_mut(), SERVICE, CpuId::BOOT).unwrap();
        let session = Session::new(client, &buffers, &names, scratch, WINDOW)?;
        let mut shell = Supervisor::<Shell<'_, '_, '_, 4, 1, 1>>::new(session)?;
        let mut out = Capture::new();

        let mut restarts = 0;
        for (tick, line) in [(1, b"ls".as_slice()), (2, b"ls docs")] {
            assert!(drive_bounded(shell.cell_mut().run(line, &mut out), 4, || {}).is_none());
            let mut hooks = Disconnect::new(&mut driver, &mut fs, CLIENT);
            if let Some(restarted) = shell.watch(tick, DEADLINE, &mut hooks) {
                restarted?;
                restarts += 1;
                assert_eq!(hooks.cancelled(), 2, "a request outlived the epoch that took it");
            }
        }

        assert_eq!(restarts, 1, "one late line is a fault, or two are not");
        assert_eq!(shell.generation(), 1);
        drive(shell.cell_mut().run(b"ls", &mut out), || {
            fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
        })?;
        assert_eq!(out.text(), "docs/\nhello.txt  11\nnote.txt  8\n");
        Ok(())
    }

    #[test]
    fn restarted_shell_loses_all() -> Result<(), ShellError> {
        let bytes = image();
        let mut scratch = [0u8; WINDOW];
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
        let mut registry = BufferRegistry::<1>::new();
        let scratch = registry.register_read_write(CLIENT, &mut scratch).unwrap();
        let buffers = RefCell::new(registry);
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let (client, mut driver) = ring.split();
        fs.publish(&mut names.borrow_mut(), SERVICE, CpuId::BOOT).unwrap();
        let session = Session::new(client, &buffers, &names, scratch, WINDOW)?;
        let mut shell = Supervisor::<Shell<'_, '_, '_, 4, 1, 1>>::new(session)?;
        let mut out = Capture::new();

        drive_bounded(shell.cell_mut().run(b"ls docs", &mut out), 2, || {
            fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
        });

        let mut hooks = Disconnect::new(&mut driver, &mut fs, CLIENT);
        shell.restart(&mut hooks)?;
        let revoked = hooks.revoked();

        assert_eq!(revoked, 1, "a stopped client kept a directory open");
        assert_eq!(shell.generation(), 1);
        assert_eq!(out.text(), "", "an unfinished listing printed half of itself");
        drive(shell.cell_mut().run(b"ls", &mut out), || {
            fs.serve(CLIENT, &mut driver, &mut buffers.borrow_mut());
        })?;
        assert_eq!(out.text(), "docs/\nhello.txt  11\nnote.txt  8\n");
        Ok(())
    }
}
