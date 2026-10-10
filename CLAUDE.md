# cooler-lcd

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): summary`, e.g. `fix(radar): ...`, `feat(usage): ...`,
`docs: ...`.

- **Types:** `feat` (new screens or behavior), `fix`, `perf`, `refactor`,
  `docs`, `test`, `chore`.
- **Scopes:** the module the change is confined to: `radar`, `usage`,
  `dashboard`, `sensors`, `device`, `render`, `theme`, `config`. Leave the
  scope out when a change spans several.
- **Body:** explain what was wrong or missing and why the change fixes it,
  wrapped at 72 columns.

Commits before `154d50a` predate this convention and are already pushed;
don't rewrite them.
