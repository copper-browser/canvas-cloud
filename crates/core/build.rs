// `sqlx::migrate!` embeds the migration files at compile time; make sure a new or edited
// migration (core identity 0001+, engine 0100+, apps 0200+, …) triggers a rebuild.
fn main() {
    println!("cargo:rerun-if-changed=../canvas-cloud/migrations");
}
