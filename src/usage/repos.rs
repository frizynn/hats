//! The repository a session ran in, so a repo's worktrees count as one
//! project. Read from the filesystem without running git: the first `.git`
//! up from the cwd is the main checkout when it is a directory, and points at
//! it when it is a file (`gitdir: <main>/.git/worktrees/<name>`). A cwd that no
//! longer exists falls back to its own folder name, and the walk stops below
//! the home folder, whose `.git` is usually a dotfiles repo.

use std::fs;
use std::path::Path;

pub struct Repo {
    /// The folder name and a hash of the root, so two repos with one name
    /// stay apart without keeping the path.
    pub key: String,
    pub label: String,
}

pub fn repo_of(cwd: &str, home: &Path) -> Repo {
    let mut dir = Path::new(cwd);
    for _ in 0..64 {
        if dir == home {
            break;
        }
        let git = dir.join(".git");
        match fs::metadata(&git) {
            Ok(meta) if meta.is_dir() => return named(dir),
            Ok(meta) if meta.is_file() => {
                let target = fs::read_to_string(&git).ok().and_then(|text| {
                    let line = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?;
                    Some(normal(&dir.join(line.trim())))
                });
                return match target {
                    Some(target) => {
                        let target = target.to_string_lossy();
                        // <main>/.git/worktrees/<name> to <main>
                        match target.rfind("/.git/worktrees/") {
                            Some(i) if i > 0 => named(Path::new(&target[..i])),
                            _ => named(Path::new(&*target).parent().unwrap_or(dir)),
                        }
                    }
                    None => named(dir),
                };
            }
            _ => {}
        }
        match dir.parent() {
            Some(up) => dir = up,
            None => break,
        }
    }
    named(Path::new(cwd))
}

/// `..` and `.` resolved without touching the disk, as a relative `gitdir`
/// needs.
fn normal(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn named(root: &Path) -> Repo {
    let label = root
        .file_name()
        .map_or_else(|| "/".to_owned(), |n| n.to_string_lossy().into_owned());
    let hash = crate::account::short_hash(&root.to_string_lossy());
    Repo {
        key: format!("{label}~{}", &hash[..6]),
        label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_counts_with_its_repo_and_a_home_git_is_not_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let main = home.join("src/app");
        fs::create_dir_all(main.join(".git/worktrees/feature")).unwrap();
        fs::create_dir_all(main.join("sub")).unwrap();
        fs::create_dir_all(home.join("trees/feature")).unwrap();
        fs::write(
            home.join("trees/feature/.git"),
            format!(
                "gitdir: {}\n",
                main.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        fs::create_dir_all(home.join(".git")).unwrap();
        fs::create_dir_all(home.join("notes")).unwrap();
        let of = |p: &str| repo_of(&home.join(p).to_string_lossy(), home);

        assert_eq!(of("trees/feature").key, of("src/app/sub").key);
        assert_eq!(of("src/app").label, "app");
        assert_eq!(of("notes").label, "notes");
        assert_eq!(of("gone/worktree").label, "worktree");
        assert_ne!(of("notes").key, of("gone/notes").key);
        // `git worktree add --relative-paths`
        fs::create_dir_all(home.join("src/app/.git/worktrees/rel")).unwrap();
        fs::create_dir_all(home.join("src/rel")).unwrap();
        fs::write(
            home.join("src/rel/.git"),
            "gitdir: ../app/.git/worktrees/rel\n",
        )
        .unwrap();
        assert_eq!(of("src/rel").key, of("src/app").key);
    }
}
