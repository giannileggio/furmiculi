---
id: fix-failing-test
description: Find out why a test fails and fix the code (not the test)
on_finish: pr
---

A test is failing. Find out why and fix it.

Failing test or command: (say which one, and paste the failure output here)

- Reproduce the failure first, and read the code under test before changing anything.
- Fix the cause in the code. Don't weaken, skip or delete the test unless it is
  itself wrong, and then say why.
- Add a regression test if the failing test didn't already cover the bug.

Done when: the whole test suite passes.

If you can't reproduce it, or the fix would mean a larger change than this,
stop and explain what you found instead of guessing.
