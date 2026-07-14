use crate::args::{Cli, CompletionShellArg};
use clap::CommandFactory;
use clap_complete::generate_to;
use clap_complete::shells::{Bash, Elvish, Fish, PowerShell, Zsh};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ArtifactError {
    #[error("cannot create artifact directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot generate completion in {path}: {source}")]
    Completion {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write artifact {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("artifact output has no file name: {0}")]
    MissingFileName(PathBuf),
}

pub(crate) fn completions(
    shell: CompletionShellArg,
    output: &Path,
) -> Result<Vec<PathBuf>, ArtifactError> {
    fs::create_dir_all(output).map_err(|source| ArtifactError::CreateDirectory {
        path: output.to_path_buf(),
        source,
    })?;
    let mut generated = Vec::new();
    match shell {
        CompletionShellArg::Bash => generated.push(generate::<Bash>(Bash, output)?),
        CompletionShellArg::Elvish => generated.push(generate::<Elvish>(Elvish, output)?),
        CompletionShellArg::Fish => generated.push(generate::<Fish>(Fish, output)?),
        CompletionShellArg::PowerShell => {
            generated.push(generate::<PowerShell>(PowerShell, output)?);
        }
        CompletionShellArg::Zsh => generated.push(generate::<Zsh>(Zsh, output)?),
        CompletionShellArg::All => {
            generated.push(generate::<Bash>(Bash, output)?);
            generated.push(generate::<Elvish>(Elvish, output)?);
            generated.push(generate::<Fish>(Fish, output)?);
            generated.push(generate::<PowerShell>(PowerShell, output)?);
            generated.push(generate::<Zsh>(Zsh, output)?);
        }
    }
    Ok(generated)
}

pub(crate) fn man_page(output: &Path) -> Result<(), ArtifactError> {
    let mut bytes = Vec::new();
    clap_mangen::Man::new(Cli::command())
        .render(&mut bytes)
        .map_err(|source| ArtifactError::Write {
            path: output.to_path_buf(),
            source,
        })?;
    atomic_write(output, &bytes)
}

pub(crate) fn atomic_write(output: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ArtifactError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })?;
    let file_name = output
        .file_name()
        .ok_or_else(|| ArtifactError::MissingFileName(output.to_path_buf()))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{}.", file_name.to_string_lossy()))
        .tempfile_in(parent)
        .map_err(|source| ArtifactError::Write {
            path: output.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| ArtifactError::Write {
            path: output.to_path_buf(),
            source,
        })?;
    temporary
        .persist(output)
        .map_err(|error| ArtifactError::Write {
            path: output.to_path_buf(),
            source: error.error,
        })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ArtifactError::Write {
            path: parent.to_path_buf(),
            source,
        })
}

fn generate<G>(generator: G, output: &Path) -> Result<PathBuf, ArtifactError>
where
    G: clap_complete::Generator,
{
    generate_to(generator, &mut Cli::command(), "rewind", output).map_err(|source| {
        ArtifactError::Completion {
            path: output.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_artifacts_come_from_live_command_definition() {
        let temp = tempfile::tempdir().unwrap();
        let files = completions(CompletionShellArg::Bash, temp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(fs::read_to_string(&files[0]).unwrap().contains("rewind"));
        let man = temp.path().join("rewind.1");
        man_page(&man).unwrap();
        let text = fs::read_to_string(man).unwrap();
        assert!(text.contains("rewind"));
        assert!(text.contains("compare"));
    }

    #[test]
    fn atomic_write_replaces_a_complete_existing_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("artifact");
        fs::write(&output, b"old").unwrap();
        atomic_write(&output, b"new").unwrap();
        assert_eq!(fs::read(output).unwrap(), b"new");
    }
}
