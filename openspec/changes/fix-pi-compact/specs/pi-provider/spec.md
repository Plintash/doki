# Spec Delta

## ADDED Requirements

### Requirement: The provider's compaction command runs instead of reaching the model

Waku SHALL offer Pi's built-in `compact` command in a Pi session's composer
even though the provider's command report omits built-ins, and a submitted
`/compact [focus]` SHALL run the provider's compaction with the focus as its
instructions rather than delivering that text to the model. Compaction SHALL
NOT abort a live turn: while a turn runs, the submission SHALL be queued or
refused as undelivered. Only Pi is affected.

#### Scenario: The command is offered

- **WHEN** a Pi session's composer index is built and the provider reports no command named `compact`
- **THEN** the index contains a built-in `compact` with its argument hint, while a project, user or skill command of the same name still owns the name

#### Scenario: A submitted compact runs the provider's compaction

- **WHEN** the user submits `/compact focus on the API` in an idle Pi session
- **THEN** the transport writes Pi's `compact` command with `focus on the API` as its instructions, and no user prompt containing the command text is delivered to the model

#### Scenario: A live turn is not aborted to compact

- **WHEN** a compact submission reaches a Pi session whose turn is still running
- **THEN** no compaction is started against that turn, and the submission is either held until the turn settles or reported as undelivered

### Requirement: Compaction is visible and settles its own turn

Waku SHALL show Pi's compaction while it runs and when it ends, with the
provider's own reason on failure, SHALL refresh the client's context meter from
the provider's post-compaction token estimate, and SHALL surface Pi's automatic
compaction through the same activity. A turn whose only work was a manual
compaction SHALL settle when that compaction ends, without a synthetic
assistant reply.

#### Scenario: Compaction is visible and the meter follows it

- **WHEN** Pi starts and finishes a manual compaction
- **THEN** an activity row shows the compaction while it runs and completes when it ends, a failure names the provider's reason, and the context meter takes the provider's post-compaction token estimate

#### Scenario: Automatic compaction is not silent

- **WHEN** Pi compacts on its own because the context crossed the threshold or overflowed
- **THEN** the same activity row explains the shrinking context, and no extra turn is settled for it

#### Scenario: A compaction-only turn settles without a reply line

- **WHEN** a turn exists only for a submitted manual compaction and the compaction ends
- **THEN** the turn settles at the compaction's end without a synthetic assistant reply, even when the compaction itself failed
