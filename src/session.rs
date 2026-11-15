//! Identity passed to Anvil-launched programs, including a nested desktop's children.
//! Keep the parent environment intact: Winit still needs its host session to connect.

pub(crate) const ENVIRONMENT: [(&str, &str); 4] = [
    ("XDG_CURRENT_DESKTOP", "Anvil"),
    ("XDG_SESSION_DESKTOP", "anvil"),
    ("XDG_SESSION_TYPE", "wayland"),
    ("DESKTOP_SESSION", "anvil"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn children_report_anvil_without_modifying_the_host_environment() {
        let parent = ENVIRONMENT.map(|(name, _)| std::env::var_os(name));
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                r#"printf '%s\n' "$XDG_CURRENT_DESKTOP" "$XDG_SESSION_DESKTOP" "$XDG_SESSION_TYPE" "$DESKTOP_SESSION""#,
            ])
            .env("XDG_CURRENT_DESKTOP", "sway")
            .env("XDG_SESSION_TYPE", "x11")
            .envs(ENVIRONMENT)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"Anvil\nanvil\nwayland\nanvil\n");
        assert_eq!(parent, ENVIRONMENT.map(|(name, _)| std::env::var_os(name)));
    }
}
