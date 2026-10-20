//! Guest side of per-box AWS access (web-identity mode of the credential broker).
//!
//! The controller signs a short-lived identity token for one box generation; the worker
//! writes it into the running guest with the files below. The standard AWS credential
//! chain then calls `AssumeRoleWithWebIdentity` itself: no AWS secret reaches the box
//! or its disk. The token and its environment live on the guest's `/run` tmpfs (RAM);
//! only the static profile hook is written to disk. See docs/AWS_ACCESS.md.
use crate::common::shell_quote;
use anyhow::{Result, ensure};

/// Guest directory holding `token` and `env` (KEY=VALUE, also a systemd EnvironmentFile).
pub const DIR: &str = "/run/ow/aws";
/// Static login-shell hook that exports the variables in `DIR/env` when present.
pub const HOOK: &str = "/etc/profile.d/ow-aws.sh";

/// Exports the role's variables when a role is attached. Login shells get it from [`HOOK`];
/// shells nothing else prepares (Alpine's `ow exec`) run it first.
pub const SOURCE: &str = "if [ -r /run/ow/aws/env ]; then set -a; . /run/ow/aws/env; set +a; fi";

/// What one box generation receives: the signed token and the role it may assume.
pub struct Delivery<'a> {
    pub token: &'a str,
    pub role_arn: &'a str,
    pub session_name: &'a str,
}

/// A compact JWS: three base64url parts, bounded in size.
pub fn token(value: &str) -> Result<()> {
    ensure!(
        (16..=4096).contains(&value.len())
            && value.split('.').count() == 3
            && value.split('.').all(|part| !part.is_empty())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid identity token"
    );
    Ok(())
}

/// AWS role session names: 2–64 of `[\w+=,.@-]`.
pub fn session_name(value: &str) -> Result<()> {
    ensure!(
        (2..=64).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_+=,.@-".contains(&b)),
        "invalid role session name"
    );
    Ok(())
}

impl Delivery<'_> {
    pub fn validate(&self) -> Result<()> {
        token(self.token)?;
        crate::credential_broker::role_arn(self.role_arn)?;
        session_name(self.session_name)
    }
}

/// One guest shell command that installs `delivery` atomically, or removes the files when
/// `None`. `root` prefixes every guest path (empty in a guest; a scratch directory in tests).
/// It prints nothing, so the token never reaches the serial console log.
pub fn install_command(root: &str, delivery: Option<&Delivery>) -> Result<String> {
    let dir = format!("{root}{DIR}");
    let Some(delivery) = delivery else {
        return Ok(format!("rm -rf -- {}", shell_quote(&dir)));
    };
    delivery.validate()?;
    // printf escapes, not literal newlines: the serial transport sends one input line.
    let env = format!(
        "printf 'AWS_ROLE_ARN=%s\\nAWS_WEB_IDENTITY_TOKEN_FILE=%s\\nAWS_ROLE_SESSION_NAME=%s\\n' {} {} {}",
        shell_quote(delivery.role_arn),
        shell_quote(&format!("{DIR}/token")),
        shell_quote(delivery.session_name)
    );
    let hook = format!(
        "printf '%s\\n' {} {}",
        shell_quote("# Open Workspaces per-box AWS access: exports only while a role is attached."),
        shell_quote(SOURCE)
    );
    let rc = format!("{root}/etc/bash.bashrc");
    let line = format!("[ -r {HOOK} ] && . {HOOK}");
    let hook_path = shell_quote(&format!("{root}{HOOK}"));
    // Developer images run commands and terminals as dev (uid 1000), who must read the token.
    Ok(format!(
        "d={d}; (umask 022; mkdir -p {parent}) && umask 077 && mkdir -p \"$d\" && printf %s {token} > \"$d/.token\" && {env} > \"$d/.env\" \
         && if [ -f {dev} ]; then chown 1000:1000 \"$d\" \"$d/.token\" \"$d/.env\"; fi \
         && mv -f \"$d/.token\" \"$d/token\" && mv -f \"$d/.env\" \"$d/env\" \
         && {{ [ -f {h} ] || {{ mkdir -p {hookdir} && {hook} > {h}.ow && chmod 644 {h}.ow && mv -f {h}.ow {h}; }}; }} \
         && {{ [ ! -f {rc} ] || grep -qF {hookname} {rc} || printf '\\n%s\\n' {line} >> {rc}; }}",
        d = shell_quote(&dir),
        // Traversable by dev; the token directory itself stays 0700.
        parent = shell_quote(&format!("{root}/run/ow")),
        token = shell_quote(delivery.token),
        dev = shell_quote(&format!("{root}/etc/ow-developer")),
        h = hook_path,
        hookdir = shell_quote(&format!("{root}/etc/profile.d")),
        rc = shell_quote(&rc),
        hookname = shell_quote(HOOK),
        line = shell_quote(&line),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJl";
    const ROLE: &str = "arn:aws:iam::111122223333:role/example/box-reader";

    #[test]
    fn refuses_values_that_could_escape_the_files() {
        for (token, role, session) in [
            ("a.b", ROLE, "ow-x"),
            ("eyJ.eyJ.sig$(id)", ROLE, "ow-x"),
            (TOKEN, "arn:aws:iam::111122223333:role/x;id", "ow-x"),
            (TOKEN, "arn:aws:iam::111122223333:role/*", "ow-x"),
            (TOKEN, "arn:aws:iam::1111:role/x", "ow-x"),
            (TOKEN, ROLE, "ow x"),
            (TOKEN, ROLE, &"a".repeat(65)),
        ] {
            let delivery = Delivery {
                token,
                role_arn: role,
                session_name: session,
            };
            assert!(
                install_command("", Some(&delivery)).is_err(),
                "{token} {role} {session}"
            );
        }
    }

    #[test]
    fn installs_atomically_then_removes() {
        let root = std::env::temp_dir().join(format!(
            "ow-web-identity-{}",
            crate::common::nonce().unwrap()
        ));
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/bash.bashrc"), "# distro rc\n").unwrap();
        let root_text = root.to_str().unwrap();
        let delivery = Delivery {
            token: TOKEN,
            role_arn: ROLE,
            session_name: "ow-0123",
        };
        let run = |command: &str| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.stdout.is_empty(), "the command must print nothing");
        };
        let install = install_command(root_text, Some(&delivery)).unwrap();
        assert!(!install.contains('\n'), "one serial input line");
        run(&install);
        run(&install);
        let dir = root.join("run/ow/aws");
        assert_eq!(std::fs::read_to_string(dir.join("token")).unwrap(), TOKEN);
        let env = std::fs::read_to_string(dir.join("env")).unwrap();
        assert_eq!(
            env,
            format!(
                "AWS_ROLE_ARN={ROLE}\nAWS_WEB_IDENTITY_TOKEN_FILE=/run/ow/aws/token\nAWS_ROLE_SESSION_NAME=ow-0123\n"
            )
        );
        use std::os::unix::fs::PermissionsExt;
        for file in ["token", "env"] {
            assert_eq!(
                std::fs::metadata(dir.join(file))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // dev must be able to reach its own token directory.
        assert_eq!(
            std::fs::metadata(dir.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let hook = root.join("etc/profile.d/ow-aws.sh");
        assert_eq!(
            std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let rc = std::fs::read_to_string(root.join("etc/bash.bashrc")).unwrap();
        assert_eq!(
            rc.lines().filter(|l| l.contains("ow-aws.sh")).count(),
            1,
            "{rc}"
        );
        assert!(
            std::fs::read_dir(&dir).unwrap().count() == 2,
            "no staging files left"
        );
        run(&install_command(root_text, None).unwrap());
        assert!(!dir.exists());
        // The hook stays and exports nothing once the role is gone.
        assert!(hook.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
