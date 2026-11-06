---
id: update-dependencies
description: Update dependencies to their latest compatible versions and check nothing broke
on_finish: pr
---

Update this project's dependencies to their latest compatible versions.

- Use the project's own package manager and lockfile; don't switch tools.
- Stay within the version ranges the project declares, unless a newer major
  version is needed for a security fix. Leave other major upgrades alone and
  list them in the final message.
- Run the tests and the build after updating.

Done when: the tests and the build pass with the updated lockfile.

If an update breaks something you can't fix quickly, leave that dependency
where it was and say which one and why.
