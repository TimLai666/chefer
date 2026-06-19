//! Windows WHP host helper skeleton.
//!
//! This binary intentionally does not boot a VM yet. It pins the CLI contract
//! that the Rust runtime will use once the Windows Hypervisor Platform device
//! model is implemented.

use std::path::PathBuf;

const EXIT_USAGE: i32 = 64;
const EXIT_UNAVAILABLE: i32 = 69;

fn main() {
    let code = match run(std::env::args().skip(1)) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            if err.starts_with("usage:") {
                EXIT_USAGE
            } else {
                EXIT_UNAVAILABLE
            }
        }
    };
    std::process::exit(code);
}

fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let args: Vec<String> = args.into_iter().collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }

    let request = HelperRequest::parse(args)?;
    Err(format!(
        "chefer-whp-helper is a contract skeleton and does not boot a VM yet. \
         Requested kernel={}, initramfs={}, bundle={}, data={}, cpus={}, memory={}MiB. \
         Use the WSL2 backend today.",
        request.kernel.display(),
        request.initramfs.display(),
        request.bundle_dir.display(),
        request.data_dir.display(),
        request.cpus,
        request.memory_mib
    ))
}

fn print_help() {
    println!(
        "chefer-whp-helper (contract skeleton)\n\
         \n\
         Usage:\n\
           chefer-whp-helper \\\n\
             --kernel <path> --initramfs <path> --cmdline <text> \\\n\
             --bundle-dir <path> --data-dir <path> --cpus <n> --memory-mib <n>\n\
         \n\
         The future helper will boot the Chefer Linux appliance through Windows \
         Hypervisor Platform and stream guest console markers to stdout."
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HelperRequest {
    kernel: PathBuf,
    initramfs: PathBuf,
    cmdline: String,
    bundle_dir: PathBuf,
    data_dir: PathBuf,
    cpus: u16,
    memory_mib: u64,
}

impl HelperRequest {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut parser = ArgParser::new(args);
        let request = HelperRequest {
            kernel: parser.path("--kernel")?,
            initramfs: parser.path("--initramfs")?,
            cmdline: parser.value("--cmdline")?,
            bundle_dir: parser.path("--bundle-dir")?,
            data_dir: parser.path("--data-dir")?,
            cpus: parse_cpus(&parser.value("--cpus")?)?,
            memory_mib: parse_memory_mib(&parser.value("--memory-mib")?)?,
        };
        parser.finish()?;
        Ok(request)
    }
}

struct ArgParser {
    args: Vec<String>,
}

impl ArgParser {
    fn new(args: Vec<String>) -> Self {
        Self { args }
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        let Some(pos) = self.args.iter().position(|arg| arg == flag) else {
            return Err(format!("usage: missing required argument {flag}"));
        };
        self.args.remove(pos);
        if pos >= self.args.len() {
            return Err(format!("usage: {flag} requires a value"));
        }
        let value = self.args.remove(pos);
        if value.starts_with("--") {
            return Err(format!("usage: {flag} requires a value"));
        }
        Ok(value)
    }

    fn path(&mut self, flag: &str) -> Result<PathBuf, String> {
        Ok(PathBuf::from(self.value(flag)?))
    }

    fn finish(self) -> Result<(), String> {
        if self.args.is_empty() {
            Ok(())
        } else {
            Err(format!("usage: unexpected argument {}", self.args[0]))
        }
    }
}

fn parse_cpus(value: &str) -> Result<u16, String> {
    let cpus = value
        .parse::<u16>()
        .map_err(|_| format!("usage: --cpus must be a positive integer, got {value}"))?;
    if cpus == 0 {
        return Err("usage: --cpus must be at least 1".to_string());
    }
    Ok(cpus)
}

fn parse_memory_mib(value: &str) -> Result<u64, String> {
    let memory_mib = value
        .parse::<u64>()
        .map_err(|_| format!("usage: --memory-mib must be a positive integer, got {value}"))?;
    if memory_mib < 512 {
        return Err("usage: --memory-mib must be at least 512".to_string());
    }
    Ok(memory_mib)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_args() -> Vec<String> {
        [
            "--kernel",
            "vm/vmlinuz",
            "--initramfs",
            "vm/initramfs",
            "--cmdline",
            "console=ttyS0",
            "--bundle-dir",
            "bundle",
            "--data-dir",
            "data",
            "--cpus",
            "2",
            "--memory-mib",
            "1024",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn parses_contract_arguments() {
        let req = HelperRequest::parse(valid_args()).unwrap();

        assert_eq!(req.kernel, PathBuf::from("vm/vmlinuz"));
        assert_eq!(req.initramfs, PathBuf::from("vm/initramfs"));
        assert_eq!(req.cmdline, "console=ttyS0");
        assert_eq!(req.bundle_dir, PathBuf::from("bundle"));
        assert_eq!(req.data_dir, PathBuf::from("data"));
        assert_eq!(req.cpus, 2);
        assert_eq!(req.memory_mib, 1024);
    }

    #[test]
    fn rejects_missing_required_argument() {
        let mut args = valid_args();
        args.drain(0..2);

        let err = HelperRequest::parse(args).unwrap_err();
        assert!(err.contains("--kernel"));
    }

    #[test]
    fn rejects_unexpected_argument() {
        let mut args = valid_args();
        args.push("--extra".to_string());

        let err = HelperRequest::parse(args).unwrap_err();
        assert!(err.contains("unexpected argument --extra"));
    }

    #[test]
    fn rejects_zero_cpus_and_tiny_memory() {
        assert!(parse_cpus("0").unwrap_err().contains("at least 1"));
        assert!(
            parse_memory_mib("128")
                .unwrap_err()
                .contains("at least 512")
        );
    }
}
