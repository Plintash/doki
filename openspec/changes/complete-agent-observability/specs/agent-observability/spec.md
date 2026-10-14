# Agent observability

## ADDED Requirements

### Requirement: Compact discovery and context

The debug automation surface SHALL describe its read operations, state sections,
verb arguments and effects, and identity domains. Ordinary tree, pointer-state,
discovery, bare state and action-result observations SHALL not serialize transcript bodies
unless explicitly requested.

#### Scenario: A large conversation does not prevent diagnosis

- **WHEN** the selected conversation exceeds the full-observation size limit
- **THEN** discovery, tree, composer reads, semantic actions and their failure
  evidence still return useful compact state
- **AND** explicitly requesting oversized full history fails by name

### Requirement: Useful cached runtime facts

Selectable state SHALL expose provider installation/enablement and cached model
catalogs, discovery progress, complete pending permission/input choices, and the
selected workspace identity and cached Git/review state. It SHALL distinguish
not requested, loading, successful and failed queries where the owner tracks
those states. Installation SHALL not be presented as authenticated model access.
Observation SHALL perform no provider, Git, filesystem or network probing.

#### Scenario: Explain a blocked submission

- **WHEN** no enabled provider is available or an approval is pending
- **THEN** an agent can obtain the underlying state and actual available choices
  without guessing IDs or attempting a submission

#### Scenario: Cached workspace state remains honest

- **WHEN** a background Git query completes or fails
- **THEN** requested workspace observations update through the existing event
  stream and identify cached or stale evidence rather than claiming a fresh scan

### Requirement: Bounded explicit-conversation history

An agent SHALL be able to read paged history metadata and bounded text slices
for an explicit conversation, preserving message/activity order and native stored item
identity. Responses SHALL give continuation, full field length and live versus
persisted provenance. Reads SHALL not open the conversation, start a provider,
or mutate its draft. An unloaded shell SHALL never be reported as empty history.

#### Scenario: A single large tool result is readable

- **WHEN** a message or tool field exceeds the snapshot limit
- **THEN** successive bounded UTF-8-safe slices reconstruct the field exactly
- **AND** previews explicitly report omitted text and a continuation locator

#### Scenario: Legacy reasoning has no stored activity ID

- **WHEN** a persisted legacy reasoning block needs a derived identity
- **THEN** the index marks it `legacyDerived` and tells callers to re-index
  after normal hydration/migration changes its identity

#### Scenario: Unselected history stays unselected

- **WHEN** an agent reads a persisted conversation whose detail is not loaded
- **THEN** the response is labeled persisted and the current selection and
  composer remain unchanged

### Requirement: Existing debug-only security boundary

The new reads SHALL use the authenticated daemon protocol and SHALL remain
unavailable in release builds. Discovery, context and errors SHALL not expose
transport credentials or create a second control plane.

#### Scenario: Release has no inspection surface

- **WHEN** a release daemon or app receives an inspection request
- **THEN** it refuses the debug operation without registering an executor
