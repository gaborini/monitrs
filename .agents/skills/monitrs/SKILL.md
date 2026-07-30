```markdown
# monitrs Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches the core development patterns and conventions used in the `monitrs` Rust codebase. You'll learn about file naming, import/export styles, commit practices, and how to write and organize tests. This guide is ideal for contributors aiming for consistency and maintainability in the project.

## Coding Conventions

### File Naming
- **Style:** camelCase
- **Example:**  
  ```text
  monitorManager.rs
  eventHandler.rs
  ```

### Import Style
- **Style:** Relative imports
- **Example:**
  ```rust
  mod utils;
  use crate::monitorManager::Monitor;
  ```

### Export Style
- **Style:** Named exports
- **Example:**
  ```rust
  pub struct Monitor { /* ... */ }
  pub fn start_monitoring() { /* ... */ }
  ```

### Commit Patterns
- **Type:** Freeform messages, with occasional `release` prefixes.
- **Example:**
  ```
  release: v1.2.0
  Fix monitor event bug
  ```

## Workflows

### Release Workflow
**Trigger:** When preparing a new version for release  
**Command:** `/release`

1. Update version numbers and documentation as needed.
2. Commit changes with a message prefixed by `release:`, e.g., `release: v1.2.0`.
3. Tag the commit with the release version.
4. Push to the main repository.
5. Announce the release if required.

### Adding a New Module
**Trigger:** When introducing new functionality  
**Command:** `/add-module`

1. Create a new file using camelCase naming (e.g., `newFeature.rs`).
2. Implement the module logic.
3. Use relative imports to integrate with existing code.
4. Export structs and functions using named exports.
5. Write corresponding tests in a `*.test.*` file.

## Testing Patterns

- **Framework:** Not explicitly detected; use Rust's built-in test framework.
- **File Pattern:** Test files are named with the pattern `*.test.*`.
- **Example:**
  ```rust
  // monitorManager.test.rs
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn test_monitor_start() {
          // Test logic here
      }
  }
  ```
- Place tests in files like `moduleName.test.rs` alongside the module or in a dedicated tests directory.

## Commands
| Command      | Purpose                                  |
|--------------|------------------------------------------|
| /release     | Prepare and commit a new release version |
| /add-module  | Scaffold and integrate a new module      |
```
