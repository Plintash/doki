# Message annotations

## Purpose

Lets a user point at exact spans of an assistant reply and attach an optional
instruction to each, so a follow-up message can reference precise text instead of
an unattributed paste, and so the transcript keeps a mark wherever the user
already commented.

## ADDED Requirements

### Requirement: Creating an annotation from a selection

The app SHALL let a user create an annotation from a non-empty text selection
inside an assistant message through an explicit action, reachable from the
message's context menu and through a keyboard shortcut. The annotation SHALL
record the selected text and the position of that text inside the source message.
Selecting text without invoking the action MUST NOT create an annotation.

#### Scenario: Create from a selection

- **WHEN** the user selects part of an assistant message and invokes the annotate action
- **THEN** an annotation containing the selected text appears in the composer, and the source span is marked in the transcript

#### Scenario: Selection alone creates nothing

- **WHEN** the user selects text and then copies it or clicks elsewhere
- **THEN** no annotation is created

### Requirement: Annotation creation while a reply is streaming

The app MUST refuse to create annotations while the annotated reply is still
streaming, and MUST make the refusal visible rather than silently ignoring the
action.

#### Scenario: Annotating a streaming reply

- **WHEN** the user selects text inside a reply that is still streaming and invokes the annotate action
- **THEN** no annotation is created and the app indicates that the reply must finish first

#### Scenario: Annotating a finished reply after it streamed

- **WHEN** the same reply has stopped streaming
- **THEN** the annotate action creates an annotation as normal

### Requirement: Staged annotations in the composer

Staged annotations SHALL appear in the composer as a labelled row showing how many
are staged, positioned above attachment tiles. The row SHALL expand into a stacked
list with one card per annotation, in creation order, each showing the quoted text
and, when present, the user's comment, and each removable without discarding the
others. Hovering the label SHALL show a read-only preview of the annotations that
contains no interactive controls.

#### Scenario: Label sits above attachments

- **WHEN** a message has both staged annotations and attachment files
- **THEN** the annotation label row appears above the attachment tiles in the composer

#### Scenario: Expanding and collapsing the list

- **WHEN** the user activates the label
- **THEN** the stacked cards are shown, and activating it again hides them

#### Scenario: Hover preview is read only

- **WHEN** the user hovers the annotation label
- **THEN** a preview of the quotes and comments is shown with no buttons or fields inside it

#### Scenario: Removing one card leaves the rest

- **WHEN** the user removes the second of three staged annotations
- **THEN** two annotations remain staged, and the transcript mark for the removed one is gone

### Requirement: Transcript marks for staged annotations

Each staged annotation SHALL mark its span in the source message with a wash over
the annotated range plus a badge in the message gutter carrying the same number as
its card in the composer. The number SHALL be the annotation's position in the
composer list. Removing an annotation SHALL remove its mark. Badges MUST NOT be
placed inline in the text and MUST NOT rely on colour alone.

#### Scenario: Staged annotation is numbered

- **WHEN** an annotation is the third staged annotation
- **THEN** its transcript mark shows the number 3, matching the composer card

#### Scenario: Deleting a card removes its mark

- **WHEN** the user removes a staged annotation
- **THEN** its wash and badge disappear from the transcript immediately

### Requirement: Jumping from a card to its span

Activating a card, by click or by keyboard, SHALL scroll the transcript to the
annotated span and briefly highlight it. This SHALL work when the source message is
off screen and when the source message is taller than the viewport. The highlight
SHALL be decorative motion and SHALL be suppressed when the system reduce-motion
setting is on, leaving the scroll and the badge as the signal.

#### Scenario: Jump to an off-screen mark

- **WHEN** the user activates a card whose source span is far above the viewport
- **THEN** the transcript scrolls so the marked span is visible and that span is briefly highlighted

#### Scenario: Jump inside a message taller than the viewport

- **WHEN** the marked span sits in the middle of a message taller than the viewport
- **THEN** the transcript lands on the marked span instead of the top of the message

#### Scenario: Reduce motion

- **WHEN** the system reduce-motion setting is on and the user activates a card
- **THEN** the transcript scrolls to the span with no animated highlight

### Requirement: Annotations are sent with the message and persist

Sending SHALL include the staged annotations with the user's message. The sent
user message SHALL retain its annotations and present them through a compact
indicator that expands on demand. Transcript marks for sent annotations SHALL
remain, and MUST NOT keep the draft's numbering once the message is sent.

#### Scenario: Sending with annotations

- **WHEN** the user sends two staged annotations with a message
- **THEN** the sent user message shows an indicator for two annotations, and expanding it shows both quotes and their comments

#### Scenario: Numbering is released after send

- **WHEN** a message with numbered annotations has been sent
- **THEN** the transcript marks remain visible but no longer display draft numbers

#### Scenario: Marks survive a reload

- **WHEN** the app is restarted and the session is reopened
- **THEN** the sent message still shows its annotations and the transcript still marks the annotated spans

### Requirement: Editing and resending restores the annotations

Reopening a sent message for editing SHALL restore its annotations as staged
annotations in the composer. When the annotated source message no longer exists,
the restored annotations SHALL keep their quote and comment, and simply have no
transcript mark.

#### Scenario: Edit restores annotations

- **WHEN** the user edits a message that carried annotations
- **THEN** those annotations are staged again in the composer with the same quotes and comments

#### Scenario: Source reply no longer exists

- **WHEN** an annotation is restored whose source message was removed by a rewind
- **THEN** the annotation still stages with its quote and comment, and no transcript mark is drawn

### Requirement: Staged annotations belong to the draft

Staged annotations SHALL persist as part of the composer draft for their session,
so switching sessions and returning, or restarting the app, restores them.

#### Scenario: Switching sessions and returning

- **WHEN** the user stages annotations, switches to another session, and comes back
- **THEN** the annotations are still staged with their quotes and comments

### Requirement: Overlapping selections extend an existing annotation

Creating an annotation whose span overlaps an existing annotation on the same
message SHALL extend that annotation instead of adding a second mark to the same
passage. Re-selecting a span that is already annotated exactly SHALL reveal the
existing card instead of creating another.

#### Scenario: Overlapping selection extends

- **WHEN** the user annotates a sentence and then annotates a span overlapping it
- **THEN** one annotation covers the union of both spans, with a single badge

#### Scenario: Exact re-selection reveals instead of duplicating

- **WHEN** the user selects exactly the text already annotated and invokes the annotate action
- **THEN** the existing annotation's card is revealed and no new annotation is created

### Requirement: Anchors survive re-rendering

An annotation SHALL keep pointing at the same text when the source message is
rendered again, including after an app update that changes how message text is
normalized. When the annotated text can no longer be located in the source message,
the mark SHALL be omitted and the annotation MUST keep the quote it captured.

#### Scenario: Re-render keeps the mark on the same text

- **WHEN** a message with a staged annotation is rendered again
- **THEN** the mark still covers the text that was annotated

#### Scenario: Text cannot be located

- **WHEN** the annotated text is no longer findable in the source message
- **THEN** no mark is drawn, and the annotation's quote and comment remain available
