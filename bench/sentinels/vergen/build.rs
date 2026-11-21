use vergen_gitcl::{Emitter, Gitcl};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gitcl = Gitcl::all_git();
    Emitter::default().fail_on_error().add_instructions(&gitcl)?.emit()?;
    Ok(())
}
