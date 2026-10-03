//! `xil setup chatterbox` — build the local-model venv the Chatterbox Turbo
//! worker runs under.
//!
//! Rust-only for now, so it is dispatched from `main.rs` rather than listed
//! in `COMMANDS`, which must match the Python CLI command for command.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, ValueEnum};
use xil_core::log;
use xil_core::workspace::{code_root, workspace_root};

const VENV: &str = "venv-chatterbox";
const TORCH: [&str; 2] = ["torch==2.6.0", "torchaudio==2.6.0"];
const PACKAGES: [&str; 2] = ["chatterbox-tts", "pydub"];
const VERIFY: &str = "import torch, torchaudio, pydub; \
                      from chatterbox.tts_turbo import ChatterboxTurboTTS; \
                      print(torch.cuda.is_available())";

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    /// Chatterbox Turbo local TTS (venv-chatterbox)
    Chatterbox,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum Device {
    Auto,
    Cuda,
    Cpu,
}

#[derive(Parser)]
#[command(
    name = "xil-setup",
    about = "Create and verify the virtual environment a local ML worker runs \
             under. 'chatterbox' builds venv-chatterbox with PyTorch (CUDA \
             wheels when an NVIDIA GPU is found, CPU wheels otherwise) and \
             chatterbox-tts, then checks that it imports.",
    after_help = "The venv is created in --dir, else $XIL_CODEROOT, else the \
                  workspace root: the places 'xil produce' looks for it. Uses \
                  uv when it is on PATH, otherwise python -m venv and pip. \
                  Running it again on a working venv does nothing."
)]
struct Args {
    /// what to set up
    #[arg(value_enum)]
    target: Target,
    /// PyTorch build to install (auto: CUDA if nvidia-smi works, else CPU)
    #[arg(long, value_enum, default_value = "auto")]
    device: Device,
    /// PyTorch CUDA wheel index tag
    #[arg(long, default_value = "cu124", value_name = "TAG")]
    cuda_index: String,
    /// directory to create venv-chatterbox in
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
    /// Python version for the venv (chatterbox-tts needs 3.10-3.13 for torch 2.6)
    #[arg(long, default_value = "3.13", value_name = "VER")]
    python: String,
    /// delete and rebuild an existing venv
    #[arg(long)]
    force: bool,
    /// print the commands without running them
    #[arg(long)]
    dry_run: bool,
}

/// What builds the venv.
#[derive(Debug, Clone)]
enum Installer {
    Uv(PathBuf),
    /// A base interpreter for `-m venv`; pip inside the venv does the rest.
    Pip(PathBuf),
}

#[derive(Debug)]
struct Step {
    label: &'static str,
    program: PathBuf,
    args: Vec<String>,
}

impl Step {
    fn new(label: &'static str, program: &Path, args: &[&str]) -> Self {
        Step {
            label,
            program: program.to_path_buf(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn display(&self) -> String {
        let mut s = self.program.display().to_string();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

fn venv_python(venv: &Path) -> PathBuf {
    venv.join("bin").join("python3")
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn find_installer(python_ver: &str) -> Option<Installer> {
    if let Some(uv) = find_on_path("uv") {
        return Some(Installer::Uv(uv));
    }
    find_on_path(&format!("python{python_ver}"))
        .or_else(|| find_on_path("python3"))
        .map(Installer::Pip)
}

fn detect_cuda() -> bool {
    Command::new("nvidia-smi")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn use_cuda(device: Device, detected: bool) -> bool {
    match device {
        Device::Auto => detected,
        Device::Cuda => true,
        Device::Cpu => false,
    }
}

/// Where the venv goes: `--dir`, else `$XIL_CODEROOT`, else the workspace.
/// The same places `resolve_venv_python` searches.
fn target_dir(
    explicit: Option<PathBuf>,
    code_root: Option<PathBuf>,
    workspace: PathBuf,
) -> PathBuf {
    explicit.or(code_root).unwrap_or(workspace)
}

fn index_url(cuda: bool, cuda_index: &str) -> String {
    let tag = if cuda { cuda_index } else { "cpu" };
    format!("https://download.pytorch.org/whl/{tag}")
}

fn plan(
    installer: &Installer,
    venv: &Path,
    python_ver: &str,
    cuda: bool,
    cuda_index: &str,
) -> Vec<Step> {
    let venv_s = venv.to_string_lossy().into_owned();
    let venv_s = venv_s.as_str();
    let py = venv_python(venv);
    let py_s = py.to_string_lossy().into_owned();
    let py_s = py_s.as_str();
    let index = index_url(cuda, cuda_index);
    let index = index.as_str();
    match installer {
        Installer::Uv(uv) => {
            let mut torch_args = vec!["pip", "install", "--python", py_s];
            torch_args.extend(TORCH);
            torch_args.extend(["--index-url", index]);
            let mut pkg_args = vec!["pip", "install", "--python", py_s];
            pkg_args.extend(PACKAGES);
            vec![
                Step::new("create venv", uv, &["venv", venv_s, "--python", python_ver]),
                Step::new("install PyTorch", uv, &torch_args),
                Step::new("install chatterbox-tts", uv, &pkg_args),
            ]
        }
        Installer::Pip(base) => {
            let mut torch_args = vec!["-m", "pip", "install"];
            torch_args.extend(TORCH);
            torch_args.extend(["--index-url", index]);
            let mut pkg_args = vec!["-m", "pip", "install"];
            pkg_args.extend(PACKAGES);
            vec![
                Step::new("create venv", base, &["-m", "venv", venv_s]),
                Step::new(
                    "upgrade pip",
                    &py,
                    &["-m", "pip", "install", "--upgrade", "pip"],
                ),
                Step::new("install PyTorch", &py, &torch_args),
                Step::new("install chatterbox-tts", &py, &pkg_args),
            ]
        }
    }
}

/// `Some(cuda_available)` when the venv imports everything the worker needs.
fn verify(python: &Path) -> Option<bool> {
    if !python.exists() {
        return None;
    }
    let out = Command::new(python).args(["-c", VERIFY]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&out.stdout).lines().last()?.trim() {
        "True" => Some(true),
        "False" => Some(false),
        _ => None,
    }
}

struct Opts {
    venv: PathBuf,
    python_ver: String,
    cuda: bool,
    cuda_index: String,
    force: bool,
    dry_run: bool,
    coderoot_set: bool,
}

fn execute(opts: &Opts, installer: Option<Installer>, out: &mut impl Write) -> anyhow::Result<i32> {
    let python = venv_python(&opts.venv);

    if !opts.force && !opts.dry_run {
        if let Some(cuda) = verify(&python) {
            writeln!(
                out,
                "{} is already set up ({}).",
                opts.venv.display(),
                device_name(cuda)
            )?;
            writeln!(out, "Use --force to rebuild it.")?;
            return Ok(0);
        }
    }

    let Some(installer) = installer else {
        log::error(&format!(
            "Neither uv nor python{} / python3 was found on PATH. Install uv \
             (https://docs.astral.sh/uv/) or Python 3.10-3.13, then retry.",
            opts.python_ver
        ));
        return Ok(1);
    };

    let steps = plan(
        &installer,
        &opts.venv,
        &opts.python_ver,
        opts.cuda,
        &opts.cuda_index,
    );
    writeln!(
        out,
        "Setting up {} with {} PyTorch wheels.",
        opts.venv.display(),
        device_name(opts.cuda)
    )?;

    if opts.dry_run {
        if opts.force && opts.venv.exists() {
            writeln!(out, "rm -rf {}", opts.venv.display())?;
        }
        for s in &steps {
            writeln!(out, "{}", s.display())?;
        }
        return Ok(0);
    }

    if opts.venv.exists() {
        if opts.force {
            writeln!(out, "Removing {}", opts.venv.display())?;
            fs::remove_dir_all(&opts.venv)?;
        } else {
            log::error(&format!(
                "{} exists but does not import chatterbox. Rerun with --force to rebuild it.",
                opts.venv.display()
            ));
            return Ok(1);
        }
    }

    let total = steps.len();
    for (i, s) in steps.iter().enumerate() {
        writeln!(out, "[{}/{}] {}: {}", i + 1, total, s.label, s.display())?;
        out.flush()?;
        let status = Command::new(&s.program).args(&s.args).status()?;
        if !status.success() {
            log::error(&format!("Step '{}' failed ({status}).", s.label));
            return Ok(1);
        }
    }

    let Some(cuda) = verify(&python) else {
        log::error(&format!(
            "{} was built but does not import chatterbox. Check the output above.",
            opts.venv.display()
        ));
        return Ok(1);
    };
    writeln!(
        out,
        "Verified: chatterbox imports; device {}.",
        device_name(cuda)
    )?;
    if opts.cuda && !cuda {
        log::warning(
            "CUDA wheels were installed but torch sees no GPU; Chatterbox Turbo \
             will fall back to the CPU. Check your NVIDIA driver, or try another \
             --cuda-index.",
        );
    }

    writeln!(out, "\nNext:")?;
    if !opts.coderoot_set {
        if let Some(dir) = opts.venv.parent() {
            writeln!(out, "  export XIL_CODEROOT={}", dir.display())?;
        }
    }
    writeln!(
        out,
        "  Save one clip per speaker as voice_refs/<speaker_key>.wav (over 5 seconds)."
    )?;
    writeln!(
        out,
        "  If the model is gated for your account: export HF_TOKEN=hf_... (weights download on first render)."
    )?;
    writeln!(
        out,
        "  xil produce --episode S01E01 --backend chatterbox-turbo"
    )?;
    Ok(0)
}

fn device_name(cuda: bool) -> &'static str {
    if cuda {
        "CUDA"
    } else {
        "CPU"
    }
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("setup");
    let parsed: Args = match super::parse_or_exit("xil-setup", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let Target::Chatterbox = parsed.target;

    let detected = parsed.device == Device::Auto && detect_cuda();
    let code_root = code_root();
    let opts = Opts {
        venv: target_dir(parsed.dir, code_root.clone(), workspace_root()).join(VENV),
        python_ver: parsed.python.clone(),
        cuda: use_cuda(parsed.device, detected),
        cuda_index: parsed.cuda_index,
        force: parsed.force,
        dry_run: parsed.dry_run,
        coderoot_set: code_root.is_some(),
    };
    let installer = find_installer(&parsed.python);
    execute(&opts, installer, &mut std::io::stdout().lock())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn opts(venv: PathBuf) -> Opts {
        Opts {
            venv,
            python_ver: "3.13".into(),
            cuda: false,
            cuda_index: "cu124".into(),
            force: false,
            dry_run: false,
            coderoot_set: true,
        }
    }

    #[test]
    fn device_choice() {
        assert!(use_cuda(Device::Auto, true));
        assert!(!use_cuda(Device::Auto, false));
        assert!(use_cuda(Device::Cuda, false));
        assert!(!use_cuda(Device::Cpu, true));
    }

    #[test]
    fn dir_precedence() {
        let ws = PathBuf::from("/ws");
        let cr = Some(PathBuf::from("/code"));
        let ex = Some(PathBuf::from("/x"));
        assert_eq!(target_dir(ex, cr.clone(), ws.clone()), PathBuf::from("/x"));
        assert_eq!(target_dir(None, cr, ws.clone()), PathBuf::from("/code"));
        assert_eq!(target_dir(None, None, ws), PathBuf::from("/ws"));
    }

    #[test]
    fn uv_plan_uses_cuda_or_cpu_index() {
        let uv = Installer::Uv(PathBuf::from("/bin/uv"));
        let venv = Path::new("/c/venv-chatterbox");
        let cuda = plan(&uv, venv, "3.13", true, "cu124");
        assert_eq!(cuda.len(), 3);
        assert_eq!(
            cuda[0].display(),
            "/bin/uv venv /c/venv-chatterbox --python 3.13"
        );
        assert_eq!(
            cuda[1].display(),
            "/bin/uv pip install --python /c/venv-chatterbox/bin/python3 torch==2.6.0 \
             torchaudio==2.6.0 --index-url https://download.pytorch.org/whl/cu124"
        );
        assert_eq!(
            cuda[2].display(),
            "/bin/uv pip install --python /c/venv-chatterbox/bin/python3 chatterbox-tts pydub"
        );
        let cpu = plan(&uv, venv, "3.13", false, "cu124");
        assert!(cpu[1].display().ends_with("/whl/cpu"));
    }

    #[test]
    fn pip_plan_runs_inside_the_venv() {
        let pip = Installer::Pip(PathBuf::from("/usr/bin/python3"));
        let steps = plan(
            &pip,
            Path::new("/c/venv-chatterbox"),
            "3.13",
            false,
            "cu124",
        );
        assert_eq!(steps.len(), 4);
        assert_eq!(
            steps[0].display(),
            "/usr/bin/python3 -m venv /c/venv-chatterbox"
        );
        assert!(steps[1..]
            .iter()
            .all(|s| s.program == Path::new("/c/venv-chatterbox/bin/python3")));
        assert!(steps[2]
            .display()
            .ends_with("--index-url https://download.pytorch.org/whl/cpu"));
    }

    #[test]
    fn dry_run_runs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls");
        let uv = tmp.path().join("uv");
        script(&uv, &format!("echo \"$@\" >> {}", log.display()));
        let mut o = opts(tmp.path().join(VENV));
        o.dry_run = true;
        let mut out = Vec::new();
        let code = execute(&o, Some(Installer::Uv(uv)), &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(!log.exists());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("pip install"));
        assert!(!tmp.path().join(VENV).exists());
    }

    #[test]
    fn working_venv_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(VENV);
        script(&venv_python(&venv), "echo True");
        let mut out = Vec::new();
        let code = execute(&opts(venv), None, &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("already set up (CUDA)"));
    }

    #[test]
    fn builds_with_fake_uv_then_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(VENV);
        let log = tmp.path().join("calls");
        // `uv venv <dir>` drops a python3 that passes the import check.
        let uv = tmp.path().join("uv");
        script(
            &uv,
            &format!(
                "echo \"$@\" >> {log}\nif [ \"$1\" = venv ]; then mkdir -p \"$2/bin\"; \
                 printf '#!/bin/sh\\necho False\\n' > \"$2/bin/python3\"; chmod +x \"$2/bin/python3\"; fi",
                log = log.display()
            ),
        );
        let mut out = Vec::new();
        let code = execute(&opts(venv.clone()), Some(Installer::Uv(uv)), &mut out).unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 3);
        assert!(calls.contains("chatterbox-tts pydub"));
        assert!(String::from_utf8(out).unwrap().contains("Verified"));
    }

    #[test]
    fn broken_venv_needs_force() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(VENV);
        script(&venv_python(&venv), "exit 1");
        let mut out = Vec::new();
        let code = execute(
            &opts(venv),
            Some(Installer::Uv(PathBuf::from("/nonexistent/uv"))),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 1);
    }
}
