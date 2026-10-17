//! The repository a session ran in, so a repo's worktrees count as one
//! project. Read from the filesystem without running git: the first `.git`
//! up from the cwd is the main checkout when it is a directory, and points at
//! it when it is a file (`gitdir: <main>/.git/worktrees/<name>`). A cwd that no
//! longer exists falls back to its own folder name, and the walk stops below
//! the home folder, whose `.git` is usually a dotfiles repo.

use std::path::Path;

pub struct Repo {
    /// The folder name and a hash of the root, so two repos with one name
    /// stay apart without keeping the path.
    pub key: String,
    pub label: String,
}

pub fn repo_of(cwd: &str, _home: &Path) -> Repo {
    Repo {
        key: cwd.to_owned(),
        label: cwd.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
