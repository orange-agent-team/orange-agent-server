fn main() {
    // sqlx::migrate! embeds files at compile time. Track the directory as well
    // as existing files so adding a migration cannot reuse a stale library.
    println!("cargo:rerun-if-changed=migrations");
}
