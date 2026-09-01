# TODO

- **Full-text search over git history.** Today's search surface
  (`global-search-and-languages.md`'s `search_tree`/Find in Path,
  and C2's `search-everywhere.md` Text tab once it lands) only searches
  the current working tree, not any commit's historical content —
  there's no way to find "which commit introduced/removed this string"
  short of manually paging through `git-log-viewer.md`'s (E3) commit
  list. Not on `docs/roadmap.md`'s tracked phase list yet; needs a
  roadmap entry (likely under Track E, alongside E3's log viewer, since
  it would reuse `ide-core`'s `git2` binding rather than the filesystem
  walk `search_tree` uses) before it's picked up by a dev-chain run.
