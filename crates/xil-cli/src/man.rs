//! Man pages from the clap definitions: `xil --generate-man [DIR]` writes
//! `xil.1` and one `xil-<command>.1` per command, the layout the Python
//! repo's `man/man1/` uses.

use std::fs;
use std::path::Path;

use crate::commands::{help_text, COMMANDS};

/// The `xil` dispatcher as a clap command: its subcommands are the table.
fn dispatcher() -> clap::Command {
    let mut cmd = clap::Command::new("xil")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Unified command-line interface for the xil podcast pipeline")
        .long_about(help_text())
        .subcommand_value_name("COMMAND");
    for c in COMMANDS {
        cmd = cmd.subcommand(clap::Command::new(c.name).about(c.description));
    }
    cmd
}

fn render(cmd: clap::Command) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd).render(&mut buf)?;
    Ok(buf)
}

/// Write every page into `dir`, creating it. Returns how many were written.
pub fn generate(dir: &Path) -> std::io::Result<usize> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("xil.1"), render(dispatcher())?)?;
    for c in COMMANDS {
        // Named for the `xil-<command>` alias, which is also the clap name.
        fs::write(
            dir.join(format!("xil-{}.1", c.name)),
            render((c.command)())?,
        )?;
    }
    Ok(COMMANDS.len() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_page_per_command_plus_the_dispatcher() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(generate(tmp.path()).unwrap(), 39);
        let parse = fs::read_to_string(tmp.path().join("xil-parse.1")).unwrap();
        assert!(parse.starts_with(".ie \\n(.g .ds Aq"), "{}", &parse[..80]);
        assert!(parse.contains("xil\\-parse"));
        let xil = fs::read_to_string(tmp.path().join("xil.1")).unwrap();
        assert!(xil.contains("sfx\\-match"));
    }
}
