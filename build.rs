use std::fs;
use std::process::Command;

fn main() {
    // Get git commit hash
    let commit_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Get build profile (debug/release)
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());

    // Set environment variables for use in the main code
    println!("cargo:rustc-env=GIT_COMMIT_HASH={}", commit_hash);
    println!("cargo:rustc-env=BUILD_PROFILE={}", profile);

    // Rebuild if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");

    // Watch the current branch ref (not hardcoded to main)
    if let Ok(head_content) = fs::read_to_string(".git/HEAD") {
        let head_content = head_content.trim();
        if head_content.starts_with("ref: ") {
            // HEAD points to a branch ref
            let ref_path = head_content.strip_prefix("ref: ").unwrap_or(head_content);
            println!("cargo:rerun-if-changed=.git/{}", ref_path);
        }
        // If HEAD is detached (contains commit hash directly),
        // the .git/HEAD watch above is sufficient
    }

    // Also watch packed-refs for cases where refs are packed
    println!("cargo:rerun-if-changed=.git/packed-refs");
}
