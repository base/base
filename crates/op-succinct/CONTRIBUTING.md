# Contributing to OP Succinct

Thanks for your interest in contributing to OP Succinct!

## Code of Conduct

The OP Succinct project adheres to the Rust Code of Conduct. This code of conduct describes the minimum behavior expected from all contributors.

## Ways to contribute
There are fundamentally three ways an individual can contribute:

1. By opening an issue: For example, if you believe that you have uncovered a bug in OP-Succinct, creating a new issue in the issue tracker is the way to report it.
2. By adding context: Providing additional context to existing issues, such as screenshots and code snippets to help resolve issues.
3. By resolving issues: Typically this is done in the form of either demonstrating that the issue reported is not a problem after all, or more often, by opening a pull request that fixes the underlying problem, in a concrete and reviewable manner.

Anybody can participate in any stage of contribution. We urge you to participate in the discussion around bugs and participate in reviewing PRs.

## Reporting Bugs

If you're experiencing an issue, please open a [GitHub Issue](https://github.com/succinctlabs/op-succinct/issues).

## Contributions Related to Spelling and Grammar

At this time, we will not be accepting contributions that only fix spelling or grammatical errors in documentation, code or elsewhere.

## Backporting Changes

OP Succinct maintains multiple release lines (e.g., `main` for v4.x development, `release/v3.x` for v3.x maintenance). When a fix or non-breaking feature should be applied to a maintenance branch, we use automated backporting.

### How It Works

1. **Add the label**: When creating a PR to `main`, add the `backport/v3.x` label if the change should also be applied to the v3.x release line.
2. **Merge to main**: Get your PR reviewed and merged as usual.
3. **Automation creates backport PR**: A GitHub Action automatically cherry-picks your changes and creates a PR targeting `release/v3.x`.
4. **Review and merge**: Review the backport PR and merge it.

### When to Use Labels

| Label | When to Use |
|-------|-------------|
| `backport/v3.x` | Bug fixes, non-breaking features, documentation updates that should also be in v3.x |
| `no-backport` | Breaking changes, features that depend on v4.x-only code, changes not applicable to v3.x |

### Handling Conflicts

If the cherry-pick fails due to conflicts:
1. The automation will post a comment on your original PR explaining the conflict.
2. You'll need to manually cherry-pick and resolve conflicts:
   ```bash
   git checkout release/v3.x
   git checkout -b backport/your-pr-number
   git cherry-pick <commit-sha>
   # Resolve conflicts
   git push origin backport/your-pr-number
   ```
3. Open a PR from your backport branch to `release/v3.x`.

### Changes Requiring Modification

Some changes may need modification when backported (e.g., different API names between versions). In these cases:
1. Do not use the `backport/v3.x` label.
2. Manually cherry-pick and adjust the code as needed.
3. Open a PR with the modified backport.

*Adapted from the [Reth contributing guide](https://github.com/paradigmxyz/reth/blob/main/CONTRIBUTING.md).*  


