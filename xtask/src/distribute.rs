use std::{
    env,
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
};

use flate2::{Compression, write::GzEncoder};
use time::OffsetDateTime;
use xshell::{Shell, cmd};
use zip::{DateTime, ZipWriter, write::SimpleFileOptions};

use crate::{
    date_iso,
    flags::{self, Malloc},
    project_root,
};

const VERSION_STABLE: &str = "0.9";
const VERSION_NIGHTLY: &str = "0.10";
const VERSION_DEV: &str = "0.11"; // keep this one in sync with `package.json`

impl flags::Dist {
    pub(crate) fn run(
        self,
        shell: &Shell,
    ) -> anyhow::Result<()> {
        let stable = shell.var("GITHUB_REF").unwrap_or_default().as_str() == "refs/heads/release";

        let project_root = project_root();
        let target = Target::get(&project_root);
        let allocator = self.allocator();
        let distribute = project_root.join("dist");
        shell.remove_path(&distribute)?;
        shell.create_dir(&distribute)?;

        if let Some(patch_version) = self.client_patch_version {
            let version = if stable {
                format!("{VERSION_STABLE}.{patch_version}")
            } else {
                // A hack to make VS Code prefer nightly over stable.
                format!("{VERSION_NIGHTLY}.{patch_version}")
            };
            dist_server(
                shell,
                &format!("{version}-standalone"),
                &target,
                allocator,
                self.zig,
            )?;
            let release_tag = if stable {
                date_iso(shell)?
            } else {
                "nightly".to_owned()
            };
            dist_client(shell, &version, &release_tag, &target)?;
        } else {
            dist_server(shell, "0.0.0-standalone", &target, allocator, self.zig)?;
        }
        Ok(())
    }
}

fn dist_client(
    shell: &Shell,
    version: &str,
    release_tag: &str,
    target: &Target,
) -> anyhow::Result<()> {
    let bundle_path = Path::new("editors").join("code").join("server");
    shell.create_dir(&bundle_path)?;
    shell.copy_file(&target.server_path, &bundle_path)?;
    if let Some(symbols_path) = &target.symbols_path {
        shell.copy_file(symbols_path, &bundle_path)?;
    }

    let _d = shell.push_dir("./editors/code");

    let mut patch = Patch::new(shell, "./package.json")?;
    patch
        .replace(
            &format!(r#""version": "{VERSION_DEV}.0-dev""#),
            &format!(r#""version": "{version}""#),
        )
        .replace(
            r#""releaseTag": null"#,
            &format!(r#""releaseTag": "{release_tag}""#),
        )
        .replace(r#""title": "$generated-start""#, "")
        .replace(r#""title": "$generated-end""#, "")
        .replace(r#""enabledApiProposals": [],"#, "");
    patch.commit(shell)?;

    Ok(())
}

fn dist_server(
    shell: &Shell,
    release: &str,
    target: &Target,
    allocator: Malloc,
    zig: bool,
) -> anyhow::Result<()> {
    let _e = shell.push_env("CFG_RELEASE", release);
    let _e = shell.push_env("CARGO_PROFILE_RELEASE_LTO", "thin");

    // Uncomment to enable debug info for releases. Note that:
    //   * debug info is split on windows and macs, so it does nothing for those platforms,
    //   * on Linux, this blows up the binary size from 8MB to 43MB, which is unreasonable.
    // let _e = sh.push_env("CARGO_PROFILE_RELEASE_DEBUG", "1");

    let linux_target = target.is_linux();
    let target_name = match &target.libc_suffix {
        Some(libc_suffix) if zig => format!("{}.{libc_suffix}", target.name),
        _ => target.name.clone(),
    };
    let features = allocator.to_features();
    let command = if linux_target && zig {
        "zigbuild"
    } else {
        "build"
    };
    cmd!(shell, "cargo {command} --manifest-path ./crates/wgsl-analyzer/Cargo.toml --bin wgsl-analyzer --target {target_name} {features...} --release").run()?;

    let destination = Path::new("dist").join(&target.artifact_name);
    if target_name.contains("-windows-") {
        zip(
            &target.server_path,
            target.symbols_path.as_ref(),
            &destination.with_extension("zip"),
        )?;
    } else {
        gzip(&target.server_path, &destination.with_extension("gz"))?;
    }

    Ok(())
}

fn gzip(
    source_path: &Path,
    destination_path: &Path,
) -> anyhow::Result<()> {
    let mut encoder = GzEncoder::new(File::create(destination_path)?, Compression::best());
    let mut input = io::BufReader::new(File::open(source_path)?);
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

fn zip(
    source_path: &Path,
    symbols_path: Option<&PathBuf>,
    destination_path: &Path,
) -> anyhow::Result<()> {
    let file = File::create(destination_path)?;
    let mut writer = ZipWriter::new(BufWriter::new(file));
    writer.start_file(
        source_path.file_name().unwrap().to_str().unwrap(),
        SimpleFileOptions::default()
            .last_modified_time(
                DateTime::try_from(OffsetDateTime::from(
                    std::fs::metadata(source_path)?.modified()?,
                ))
                .unwrap(),
            )
            .unix_permissions(0o755)
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9)),
    )?;
    let mut input = io::BufReader::new(File::open(source_path)?);
    io::copy(&mut input, &mut writer)?;
    if let Some(symbols_path) = symbols_path {
        writer.start_file(
            symbols_path.file_name().unwrap().to_str().unwrap(),
            SimpleFileOptions::default()
                .last_modified_time(
                    DateTime::try_from(OffsetDateTime::from(
                        std::fs::metadata(source_path)?.modified()?,
                    ))
                    .unwrap(),
                )
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(9)),
        )?;
        let mut input = io::BufReader::new(File::open(symbols_path)?);
        io::copy(&mut input, &mut writer)?;
    }
    writer.finish()?;
    Ok(())
}

struct Target {
    name: String,
    libc_suffix: Option<String>,
    server_path: PathBuf,
    symbols_path: Option<PathBuf>,
    artifact_name: String,
}

impl Target {
    fn get(project_root: &Path) -> Self {
        let name = env::var("WA_TARGET").unwrap_or_else(|_| {
            if cfg!(target_os = "linux") {
                "x86_64-unknown-linux-gnu".to_owned()
            } else if cfg!(target_os = "windows") {
                "x86_64-pc-windows-msvc".to_owned()
            } else if cfg!(target_os = "macos") {
                "x86_64-apple-darwin".to_owned()
            } else {
                panic!("Unsupported OS, maybe try setting WA_TARGET")
            }
        });
        let (name, libc_suffix) = match name.split_once('.') {
            Some((name, libc_suffix)) => (name.to_owned(), Some(libc_suffix.to_owned())),
            None => (name, None),
        };
        let out_path = project_root.join("target").join(&name).join("release");
        let (exe_suffix, symbols_path) = if name.contains("-windows-") {
            (".exe".into(), Some(out_path.join("wgsl_analyzer.pdb")))
        } else {
            (String::new(), None)
        };
        let server_path = out_path.join(format!("wgsl-analyzer{exe_suffix}"));
        let artifact_name = format!("wgsl-analyzer-{name}{exe_suffix}");
        Self {
            name,
            libc_suffix,
            server_path,
            symbols_path,
            artifact_name,
        }
    }

    fn is_linux(&self) -> bool {
        self.name.contains("-linux-")
    }
}

struct Patch {
    path: PathBuf,
    original_contents: String,
    contents: String,
}

impl Patch {
    fn new(
        sh: &Shell,
        path: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        let path = path.into();
        let contents = sh.read_file(&path)?;
        Ok(Self {
            path,
            original_contents: contents.clone(),
            contents,
        })
    }

    fn replace(
        &mut self,
        from: &str,
        to: &str,
    ) -> &mut Self {
        assert!(self.contents.contains(from));
        self.contents = self.contents.replace(from, to);
        self
    }

    fn commit(
        &self,
        sh: &Shell,
    ) -> anyhow::Result<()> {
        sh.write_file(&self.path, &self.contents)?;
        Ok(())
    }
}

impl Drop for Patch {
    fn drop(&mut self) {
        // FIXME: find a way to bring this back
        _ = &self.original_contents;
        // write_file(&self.path, &self.original_contents).unwrap();
    }
}
