## Purpose

Defines how Warden ships immediately usable hook examples while preserving every hook a user has already authored in their selected Warden home.

## ADDED Requirements

### Requirement: Repository-owned template catalog
The Warden repository SHALL define canonical hook templates beneath `.warden/warden-hooks/<hook-name>/`, and a packaged Warden CLI SHALL carry those templates without requiring the source checkout to remain available at runtime. Generated Codex marker skills SHALL NOT be stored as template source.

#### Scenario: Packaged CLI starts outside the source checkout
- **WHEN** a packaged Warden CLI starts from a directory that does not contain the Warden repository
- **THEN** it can still reconcile every template shipped in that CLI version

### Requirement: Missing templates are installed during startup
Every Warden CLI startup SHALL check the selected Warden home for each shipped template and SHALL install a template whose destination hook directory does not exist. Reconciliation SHALL finish before initial hook discovery and generated marker-skill reconciliation.

#### Scenario: First startup installs a template
- **WHEN** Warden starts with an empty selected Warden home
- **THEN** it installs each shipped hook under `<warden-home>/warden-hooks/<hook-name>/`
- **AND** the installed hooks are discoverable during that same startup
- **AND** their marker skills are generated through the normal marker reconciliation flow

#### Scenario: Custom Warden home is selected
- **WHEN** Warden starts with a non-default Warden home
- **THEN** templates are reconciled only into that selected home

#### Scenario: Removed template is restored
- **WHEN** the destination directory for a shipped template is absent at a later startup
- **THEN** Warden installs that template again

### Requirement: Existing hook directories are preserved
Template reconciliation SHALL treat an existing destination hook directory as user-owned and SHALL NOT overwrite, merge, delete, or otherwise modify any content inside it.

#### Scenario: User changed the installed template
- **WHEN** a template's destination hook directory already exists with user changes
- **THEN** startup leaves the entire directory byte-for-byte unchanged

#### Scenario: Existing directory is incomplete
- **WHEN** a destination hook directory exists but contains only some template files
- **THEN** startup does not fill in or replace the missing files

#### Scenario: A newer CLI ships a changed template
- **WHEN** a newer Warden version starts and the destination hook directory already exists
- **THEN** the user's existing copy remains unchanged

### Requirement: Template installation failures are visible
Warden SHALL fail onboarding with a diagnostic that identifies the affected template and destination when it cannot safely install a missing template. It SHALL NOT leave that template looking like a successfully installed partial hook.

#### Scenario: Destination cannot be written
- **WHEN** Warden cannot complete installation of a missing template
- **THEN** startup reports the template installation failure
- **AND** hook discovery does not treat a partially copied template as successfully installed
