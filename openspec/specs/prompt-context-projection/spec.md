# prompt-context-projection Specification

## Purpose
Defines the single client-side path that turns staged context records — message
annotations today, other record kinds later — into the text a coding agent
receives, so every provider learns the same thing without any provider-specific
protocol.

## Requirements

### Requirement: One projection, no provider-specific protocol

Every send path that can carry annotations — a new prompt, a steering message
during a running turn, a queued message, and an edit that resends a message — SHALL
produce its prompt text through the same projection. The daemon contract SHALL
carry plain prompt text and MUST NOT carry annotations as structured data, and no
provider-specific behavior may change how annotations arrive.

#### Scenario: All send paths produce identical annotation text

- **WHEN** the same user text and the same annotations are sent as a prompt, as a steer, from the queue, and from an edit-resend
- **THEN** the annotation portion of the prompt text is identical in all four cases

#### Scenario: Every provider receives the annotation text

- **WHEN** an annotated message is sent under any supported provider
- **THEN** that provider's prompt contains the annotation text, with no provider excluded or treated differently

### Requirement: Framing and numbering

The prompt SHALL state what the annotations are: that the quoted spans reference
text in the assistant's earlier reply and that a comment, when present, is the
user's own instruction about that span. Entries SHALL be numbered so that the
numbers match the numbers the user sees, letting the user's own text refer to an
entry (for example "fix 2"). The user's own message text SHALL be reproduced
verbatim ahead of the annotation block.

#### Scenario: Numbers match what the user sees

- **WHEN** the user writes "fix 2" and the second staged annotation carries a comment
- **THEN** the prompt's second entry is the entry the user's text refers to, and it carries that comment

#### Scenario: User text is not rewritten

- **WHEN** the user's message text is sent with annotations
- **THEN** it appears verbatim in the prompt

### Requirement: Contents of an annotation entry

Each entry SHALL carry the quoted text, the comment when one exists (omitting the
field entirely when it does not), and a locator identifying which of the
assistant's earlier replies the quote came from. When the quoted span is a fragment
of the block that contains it, the entry SHALL also carry a context window around
the quote. Internal coordinates — offsets, element ordinals, message ids — MUST NOT
appear anywhere in the prompt.

#### Scenario: Comment present and absent

- **WHEN** two annotations are sent, one with a comment and one without
- **THEN** the first entry includes a comment line and the second entry contains no comment field at all

#### Scenario: Fragment selection gains context

- **WHEN** the annotated span is a fragment of the paragraph containing it
- **THEN** the entry includes a context window around the quote

#### Scenario: Whole-block selection needs no context

- **WHEN** the annotated span is the entire block
- **THEN** the entry carries the quote without a separate context window

#### Scenario: No internal coordinates leak

- **WHEN** any annotated message is projected
- **THEN** the prompt contains no byte offsets, element ordinals, or message identifiers

### Requirement: Caps and truncation are explicit

Per-quote and context-window size limits SHALL be applied, and any truncated text
SHALL be marked as truncated rather than silently cut.

#### Scenario: Oversized quote

- **WHEN** an annotation quotes more text than the per-quote limit allows
- **THEN** the prompt carries the leading part of the quote followed by a truncation marker

#### Scenario: Oversized context window

- **WHEN** the fragment's surrounding block is longer than the context limit
- **THEN** the context window is cut on both sides and marked as truncated

### Requirement: Annotations without any user text

When the user sends annotations with an empty composer, the prompt SHALL contain
the annotation block and MUST NOT invent an instruction the user did not write.

#### Scenario: Annotation-only send

- **WHEN** the user sends three annotations with nothing typed in the composer
- **THEN** the prompt contains the framing and the three entries, and no fabricated request

### Requirement: Quoted text stays data

Quoted text SHALL be delimited as a quoted value and the projection SHALL keep its
structure regardless of the quote's content. A quote containing text that resembles
the projection's own labels, or directive-like sentences such as "ignore all
previous instructions", MUST remain confined to its own field and MUST NOT be read
as structure or as an instruction outside that field.

#### Scenario: Directive-like quoted text

- **WHEN** an annotation quotes text containing directive-like sentences or lines resembling the projection's labels
- **THEN** the projection still produces one entry per annotation, with that text confined to the quote field

#### Scenario: Multi-line quote

- **WHEN** an annotation quotes several lines, including blank lines
- **THEN** the entry remains a single entry whose quote spans those lines
