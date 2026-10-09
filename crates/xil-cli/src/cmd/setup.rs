//! `xil setup` — build the venv a local ML worker runs under. Port of
//! `XILU023_setup.py`.
//!
//! * `chatterbox` — `venv-chatterbox` for the Chatterbox Turbo TTS worker:
//!   PyTorch, `chatterbox-tts` and `pydub`.
//! * `whisper` — `venv-whisper` for `xil stem-verify`: `faster-whisper`
//!   (CTranslate2, no PyTorch).
//! * `mmaudio` — `venv-mmaudio` for `--sfx-backend mmaudio`: a pinned clone
//!   of hkchengrex/MMAudio installed editable, then PyTorch re-pinned
//!   afterwards (MMAudio's unbounded `torch` pulls a CUDA 13 build).

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, ValueEnum};
use xil_core::log;
use xil_core::workspace::{code_root, workspace_root};

/// One `xil setup` target. `repo` (URL, ref) is cloned next to the venv as
/// `MMAudio` and installed editable before `packages`; `torch` is installed
/// last so it wins over whatever `packages` pulled in.
struct Spec {
    venv: &'static str,
    python: &'static str,
    torch: &'static [&'static str],
    packages: &'static [&'static str],
    label: &'static str,
    import_name: &'static str,
    verify: &'static str,
    repo: Option<(&'static str, &'static str)>,
}

const CHATTERBOX: Spec = Spec {
    venv: "venv-chatterbox",
    python: "3.13",
    torch: &["torch==2.6.0", "torchaudio==2.6.0"],
    packages: &["chatterbox-tts", "pydub"],
    label: "install chatterbox-tts",
    import_name: "chatterbox",
    verify: "import torch, torchaudio, pydub; \
             from chatterbox.tts_turbo import ChatterboxTurboTTS; \
             print(torch.cuda.is_available())",
    repo: None,
};

const WHISPER: Spec = Spec {
    venv: "venv-whisper",
    python: "3.13",
    torch: &[],
    packages: &["faster-whisper"],
    label: "install faster-whisper",
    import_name: "faster_whisper",
    verify: "import ctranslate2; from faster_whisper import WhisperModel; \
             print(ctranslate2.get_cuda_device_count() > 0)",
    repo: None,
};

/// MMAudio pins numpy<2.1, which has no Python 3.13 wheels: default to 3.12.
const MMAUDIO: Spec = Spec {
    venv: "venv-mmaudio",
    python: "3.12",
    torch: &["torch==2.6.0", "torchaudio==2.6.0", "torchvision==0.21.0"],
    packages: &["pydub"],
    label: "install MMAudio",
    import_name: "mmaudio",
    verify: "import torch, torchaudio, pydub; \
             from mmaudio.eval_utils import all_model_cfg; \
             print(torch.cuda.is_available())",
    repo: Some(("https://github.com/hkchengrex/MMAudio", "974010a")),
};

/// The checkpoint of the worker's model (`mmaudio_worker._DEFAULT_VARIANT`).
const MMAUDIO_WEIGHTS: [&str; 2] = ["weights", "mmaudio_large_44k_v2.pth"];

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum Target {
    /// Chatterbox Turbo local TTS (venv-chatterbox)
    Chatterbox,
    /// faster-whisper for xil stem-verify (venv-whisper)
    Whisper,
    /// MMAudio local SFX (venv-mmaudio)
    Mmaudio,
}

impl Target {
    fn spec(self) -> &'static Spec {
        match self {
            Target::Chatterbox => &CHATTERBOX,
            Target::Whisper => &WHISPER,
            Target::Mmaudio => &MMAUDIO,
        }
    }
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
             under. 'chatterbox' builds venv-chatterbox (PyTorch + \
             chatterbox-tts) for local TTS; 'whisper' builds venv-whisper \
             (faster-whisper) for xil stem-verify; 'mmaudio' clones MMAudio \
             and builds venv-mmaudio for local SFX. PyTorch comes as CUDA \
             wheels when an NVIDIA GPU is found, CPU wheels otherwise.",
    after_help = "The venv is created in --dir, else $XIL_CODEROOT, else the \
                  workspace root: the places the xil commands look for it. Uses \
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
    /// directory to create the venv in
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
    /// Python version for the venv (default: 3.13; 3.12 for mmaudio)
    #[arg(long, value_name = "VER")]
    python: Option<String>,
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
    label: String,
    program: PathBuf,
    args: Vec<String>,
}

impl Step {
    fn new(label: impl Into<String>, program: &Path, args: &[&str]) -> Self {
        Step {
            label: label.into(),
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

/// The directory that holds the venv (Python's `os.path.dirname`).
fn venv_parent(venv: &Path) -> &Path {
    venv.parent().unwrap_or(Path::new(""))
}

/// The MMAudio clone sits beside its venv.
fn repo_dir(venv: &Path) -> PathBuf {
    venv_parent(venv).join("MMAudio")
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

fn needs_clone(spec: &Spec, venv: &Path) -> bool {
    spec.repo.is_some() && !repo_dir(venv).is_dir()
}

fn plan(
    installer: &Installer,
    venv: &Path,
    python_ver: &str,
    cuda: bool,
    cuda_index: &str,
    spec: &Spec,
    git: &Path,
) -> Vec<Step> {
    let venv_s = venv.to_string_lossy().into_owned();
    let py = venv_python(venv);
    let py_s = py.to_string_lossy().into_owned();
    let index = index_url(cuda, cuda_index);

    let (mut steps, install_prog, install_args): (Vec<Step>, PathBuf, Vec<&str>) = match installer {
        Installer::Uv(uv) => (
            vec![Step::new(
                "create venv",
                uv,
                &["venv", &venv_s, "--python", python_ver],
            )],
            uv.clone(),
            vec!["pip", "install", "--python", &py_s],
        ),
        Installer::Pip(base) => (
            vec![
                Step::new("create venv", base, &["-m", "venv", &venv_s]),
                Step::new(
                    "upgrade pip",
                    &py,
                    &["-m", "pip", "install", "--upgrade", "pip"],
                ),
            ],
            py.clone(),
            vec!["-m", "pip", "install"],
        ),
    };
    let install_step = |label: &str, args: &[&str]| {
        let mut all = install_args.clone();
        all.extend_from_slice(args);
        Step::new(label, &install_prog, &all)
    };

    let mut torch_args: Vec<&str> = spec.torch.to_vec();
    torch_args.extend(["--index-url", &index]);
    let torch = install_step("install PyTorch", &torch_args);

    let Some((url, git_ref)) = spec.repo else {
        if !spec.torch.is_empty() {
            steps.push(torch);
        }
        steps.push(install_step(spec.label, spec.packages));
        return steps;
    };

    let repo = repo_dir(venv);
    let repo_s = repo.to_string_lossy().into_owned();
    if needs_clone(spec, venv) {
        steps.push(Step::new("clone MMAudio", git, &["clone", url, &repo_s]));
        steps.push(Step::new(
            format!("check out MMAudio {git_ref}"),
            git,
            &["-C", &repo_s, "checkout", git_ref],
        ));
    }
    let mut pkg_args = vec!["-e", repo_s.as_str()];
    pkg_args.extend_from_slice(spec.packages);
    steps.push(install_step(spec.label, &pkg_args));
    steps.push(torch);
    steps
}

/// `Some(gpu_available)` when the venv imports everything the worker needs.
fn verify(python: &Path, spec: &Spec) -> Option<bool> {
    if !python.exists() {
        return None;
    }
    let out = Command::new(python)
        .args(["-c", spec.verify])
        .output()
        .ok()?;
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
    target: Target,
    git: Option<PathBuf>,
}

fn header(opts: &Opts) -> String {
    if opts.target.spec().torch.is_empty() {
        return format!("Setting up {}.", opts.venv.display());
    }
    format!(
        "Setting up {} with {} PyTorch wheels.",
        opts.venv.display(),
        device_name(opts.cuda)
    )
}

fn no_gpu_warning(target: Target) -> String {
    let worker = match target {
        Target::Whisper => {
            return "nvidia-smi works but CTranslate2 sees no GPU; Whisper will run on \
                    the CPU (int8). Check your NVIDIA driver."
                .to_string()
        }
        Target::Mmaudio => "MMAudio",
        Target::Chatterbox => "Chatterbox Turbo",
    };
    format!(
        "CUDA wheels were installed but torch sees no GPU; {worker} will fall back \
         to the CPU. Check your NVIDIA driver, or try another --cuda-index."
    )
}

fn next_steps(opts: &Opts) -> Vec<String> {
    match opts.target {
        Target::Whisper => vec!["  xil stem-verify --episode S01E01".into()],
        Target::Mmaudio => {
            let weights = MMAUDIO_WEIGHTS
                .iter()
                .fold(venv_parent(&opts.venv).to_path_buf(), |p, s| p.join(s));
            let found = if weights.is_file() {
                format!("  Weights found: {}", weights.display())
            } else {
                format!(
                    "  Weights not found: {} (about 6 GB downloads on the first run).",
                    weights.display()
                )
            };
            vec![
                found,
                "  MMAudio weights are CC BY-NC 4.0: non-commercial use only.".into(),
                "  xil sfx --episode S01E01 --gen-sfx --sfx-backend mmaudio \
                 --mmaudio-accept-noncommercial"
                    .into(),
            ]
        }
        Target::Chatterbox => vec![
            "  Save one clip per speaker as voice_refs/<speaker_key>.wav (over 5 seconds).".into(),
            "  If the model is gated for your account: export HF_TOKEN=hf_... \
             (weights download on first render)."
                .into(),
            "  xil produce --episode S01E01 --backend chatterbox-turbo".into(),
        ],
    }
}

fn execute(opts: &Opts, installer: Option<Installer>, out: &mut impl Write) -> anyhow::Result<i32> {
    let spec = opts.target.spec();
    let python = venv_python(&opts.venv);

    if !opts.force && !opts.dry_run {
        if let Some(cuda) = verify(&python, spec) {
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

    if let Installer::Pip(base) = &installer {
        let wanted = format!("python{}", opts.python_ver);
        if base.file_name().and_then(|n| n.to_str()) != Some(wanted.as_str()) {
            log::warning(&format!(
                "python{} was not found on PATH; building with {} instead. Install uv \
                 (it fetches the right Python) if the build fails.",
                opts.python_ver,
                base.display()
            ));
        }
    }

    if needs_clone(spec, &opts.venv) && opts.git.is_none() {
        let (url, _) = spec.repo.unwrap_or_default();
        log::error(&format!(
            "git was not found on PATH; it is needed to clone {url} into {}. \
             Install git, or clone it there yourself, then retry.",
            repo_dir(&opts.venv).display()
        ));
        return Ok(1);
    }

    let git = opts.git.clone().unwrap_or_else(|| PathBuf::from("git"));
    let steps = plan(
        &installer,
        &opts.venv,
        &opts.python_ver,
        opts.cuda,
        &opts.cuda_index,
        spec,
        &git,
    );
    writeln!(out, "{}", header(opts))?;

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
                "{} exists but does not import {}. Rerun with --force to rebuild it.",
                opts.venv.display(),
                spec.import_name
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

    let Some(cuda) = verify(&python, spec) else {
        log::error(&format!(
            "{} was built but does not import {}. Check the output above.",
            opts.venv.display(),
            spec.import_name
        ));
        return Ok(1);
    };
    writeln!(
        out,
        "Verified: {} imports; device {}.",
        spec.import_name,
        device_name(cuda)
    )?;
    if opts.cuda && !cuda {
        log::warning(&no_gpu_warning(opts.target));
    }

    writeln!(out, "\nNext:")?;
    if !opts.coderoot_set {
        writeln!(
            out,
            "  export XIL_CODEROOT={}",
            venv_parent(&opts.venv).display()
        )?;
    }
    for line in next_steps(opts) {
        writeln!(out, "{line}")?;
    }
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
    let spec = parsed.target.spec();
    let python_ver = parsed.python.unwrap_or_else(|| spec.python.to_string());

    let detected = parsed.device == Device::Auto && detect_cuda();
    let code_root = code_root();
    let opts = Opts {
        venv: target_dir(parsed.dir, code_root.clone(), workspace_root()).join(spec.venv),
        python_ver,
        cuda: use_cuda(parsed.device, detected),
        cuda_index: parsed.cuda_index,
        force: parsed.force,
        dry_run: parsed.dry_run,
        coderoot_set: code_root.is_some(),
        target: parsed.target,
        git: find_on_path("git"),
    };
    let installer = find_installer(&opts.python_ver);
    execute(&opts, installer, &mut std::io::stdout().lock())
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Writing a script while another test forks can leave it "Text file
    /// busy" at exec; these tests write and run scripts, so they take turns.
    static SCRIPTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SCRIPTS.lock().unwrap_or_else(|e| e.into_inner())
    }

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
            target: Target::Chatterbox,
            git: Some(PathBuf::from("/usr/bin/git")),
        }
    }

    fn displays(steps: &[Step]) -> Vec<String> {
        steps.iter().map(Step::display).collect()
    }

    const GIT: &str = "/usr/bin/git";

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
    fn default_python_per_target() {
        assert_eq!(Target::Chatterbox.spec().python, "3.13");
        assert_eq!(Target::Whisper.spec().python, "3.13");
        assert_eq!(Target::Mmaudio.spec().python, "3.12");
    }

    #[test]
    fn uv_plan_uses_cuda_or_cpu_index() {
        let uv = Installer::Uv(PathBuf::from("/bin/uv"));
        let venv = Path::new("/c/venv-chatterbox");
        let cuda = plan(
            &uv,
            venv,
            "3.13",
            true,
            "cu124",
            &CHATTERBOX,
            Path::new(GIT),
        );
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
        let cpu = plan(
            &uv,
            venv,
            "3.13",
            false,
            "cu124",
            &CHATTERBOX,
            Path::new(GIT),
        );
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
            &CHATTERBOX,
            Path::new(GIT),
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
    fn whisper_plan_has_no_torch() {
        let uv = Installer::Uv(PathBuf::from("/bin/uv"));
        let steps = plan(
            &uv,
            Path::new("/c/venv-whisper"),
            "3.13",
            true,
            "cu124",
            &WHISPER,
            Path::new(GIT),
        );
        assert_eq!(
            displays(&steps),
            [
                "/bin/uv venv /c/venv-whisper --python 3.13",
                "/bin/uv pip install --python /c/venv-whisper/bin/python3 faster-whisper",
            ]
        );
        let pip = Installer::Pip(PathBuf::from("/usr/bin/python3.13"));
        let steps = plan(
            &pip,
            Path::new("/c/venv-whisper"),
            "3.13",
            false,
            "cu124",
            &WHISPER,
            Path::new(GIT),
        );
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1].label, "upgrade pip");
        assert_eq!(
            steps[2].display(),
            "/c/venv-whisper/bin/python3 -m pip install faster-whisper"
        );
    }

    #[test]
    fn mmaudio_plan_clones_then_repins_torch_last() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join("venv-mmaudio");
        let repo = tmp.path().join("MMAudio");
        let uv = Installer::Uv(PathBuf::from("/bin/uv"));
        let steps = plan(&uv, &venv, "3.12", true, "cu124", &MMAUDIO, Path::new(GIT));
        let py = venv_python(&venv);
        assert_eq!(
            displays(&steps),
            [
                format!("/bin/uv venv {} --python 3.12", venv.display()),
                format!(
                    "{GIT} clone https://github.com/hkchengrex/MMAudio {}",
                    repo.display()
                ),
                format!("{GIT} -C {} checkout 974010a", repo.display()),
                format!(
                    "/bin/uv pip install --python {} -e {} pydub",
                    py.display(),
                    repo.display()
                ),
                format!(
                    "/bin/uv pip install --python {} torch==2.6.0 torchaudio==2.6.0 \
                     torchvision==0.21.0 --index-url https://download.pytorch.org/whl/cu124",
                    py.display()
                ),
            ]
        );
        assert_eq!(steps[2].label, "check out MMAudio 974010a");

        // An existing clone is reused.
        fs::create_dir(&repo).unwrap();
        let steps = plan(&uv, &venv, "3.12", false, "cu124", &MMAUDIO, Path::new(GIT));
        assert_eq!(steps.len(), 3);
        assert!(steps.iter().all(|s| s.program != Path::new(GIT)));
        assert!(steps[2].display().ends_with("/whl/cpu"));
    }

    #[test]
    fn mmaudio_without_git_fails_before_building() {
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts(tmp.path().join("venv-mmaudio"));
        o.target = Target::Mmaudio;
        o.git = None;
        o.dry_run = true;
        let mut out = Vec::new();
        let code = execute(&o, Some(Installer::Uv(PathBuf::from("/bin/uv"))), &mut out).unwrap();
        assert_eq!(code, 1);
        assert!(out.is_empty());

        // With a clone in place, git is not needed.
        fs::create_dir(tmp.path().join("MMAudio")).unwrap();
        let code = execute(&o, Some(Installer::Uv(PathBuf::from("/bin/uv"))), &mut out).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn headers_and_next_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts(tmp.path().join("venv-whisper"));
        o.target = Target::Whisper;
        assert_eq!(header(&o), format!("Setting up {}.", o.venv.display()));
        o.target = Target::Mmaudio;
        o.venv = tmp.path().join("venv-mmaudio");
        assert!(header(&o).ends_with("with CPU PyTorch wheels."));
        assert!(next_steps(&o)[0].starts_with("  Weights not found: "));
        let w = tmp.path().join("weights");
        fs::create_dir(&w).unwrap();
        fs::write(w.join("mmaudio_large_44k_v2.pth"), "").unwrap();
        assert_eq!(
            next_steps(&o)[0],
            format!(
                "  Weights found: {}",
                w.join("mmaudio_large_44k_v2.pth").display()
            )
        );
    }

    #[test]
    fn dry_run_runs_nothing() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls");
        let uv = tmp.path().join("uv");
        script(&uv, &format!("echo \"$@\" >> {}", log.display()));
        let mut o = opts(tmp.path().join(CHATTERBOX.venv));
        o.dry_run = true;
        let mut out = Vec::new();
        let code = execute(&o, Some(Installer::Uv(uv)), &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(!log.exists());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("pip install"));
        assert!(!tmp.path().join(CHATTERBOX.venv).exists());
    }

    #[test]
    fn working_venv_is_left_alone() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(CHATTERBOX.venv);
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
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(CHATTERBOX.venv);
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
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(CHATTERBOX.venv);
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
