import { describe, expect, test } from 'bun:test'

import { reduceRuntimeEvent } from './event-reducer'
import { activityDisclosureSections } from './transcript-presentation'
import type { AgentSession, SequencedEvent } from './generated'

const clock = {
  nowSeconds: () => 200,
  nowMillis: () => 200_000,
  randomUUID: (() => {
    let id = 0
    return () => `00000000-0000-4000-8000-${String(++id).padStart(12, '0')}`
  })(),
}

const SUBMISSION = {
  message: 'Second prompt',
  turnId: '10000000-0000-4000-8000-000000000002',
  messageId: '20000000-0000-4000-8000-000000000002',
}

describe('session revision', () => {
  test('submission and streamed events preserve a newer issue metadata revision', () => {
    const session = { ...idleSession(), task_id: 'filed-issue', updated_at: 201 }
    const submitted = apply(session, 'promptSubmitted', SUBMISSION)
    const started = apply(submitted, 'turnStarted', null)
    const replied = apply(started, 'textDelta', 'Sure.')
    expect(replied.updated_at).toBe(201)
    expect(replied.task_id).toBe('filed-issue')
    expect(replied.last_reply_at).toBe(200)
    expect(replied.messages.at(-1)?.content).toBe('Sure.')
  })
})

describe('promptSubmitted', () => {
  test('a client following the runtime mirrors another client’s submission under its ids', () => {
    // The desktop stayed attached to the idle runtime after the first turn;
    // the phone then submitted the second prompt.
    const result = reduceRuntimeEvent(idleSession(), event('promptSubmitted', SUBMISSION), clock)
    const session = result.session

    expect(session.status).toBe('connecting')
    expect(session.turns).toHaveLength(2)
    expect(session.turns.at(-1)).toMatchObject({
      id: SUBMISSION.turnId,
      turn_count: 2,
      status: 'running',
      provider_turn_started: false,
    })
    expect(session.messages.at(-1)).toMatchObject({
      id: SUBMISSION.messageId,
      turn_id: SUBMISSION.turnId,
      role: 'user',
      content: 'Second prompt',
    })

    // The provider's start confirms that turn instead of inventing one, so
    // the reply streams under the prompt that asked for it.
    const started = apply(session, 'turnStarted', null)
    expect(started.turns).toHaveLength(2)
    expect(started.turns.at(-1)?.provider_turn_started).toBe(true)
    const replied = apply(started, 'textDelta', 'Sure.')
    expect(replied.messages.map((message) => message.role)).toEqual([
      'user',
      'assistant',
      'user',
      'assistant',
    ])
    expect(replied.messages.at(-1)?.turn_id).toBe(SUBMISSION.turnId)
  })

  test('the submitting client’s own echo changes nothing', () => {
    const session = runningSession()
    const result = reduceRuntimeEvent(
      session,
      event('promptSubmitted', {
        message: 'Go',
        turnId: SUBMISSION.turnId,
        messageId: SUBMISSION.messageId,
      }),
      clock,
    )

    expect(result.session.turns).toEqual(session.turns)
    expect(result.session.messages).toEqual(session.messages)
    expect(result.session.status).toBe('connecting')
  })

  test('a provider-started turn without a prompt receives the submitted message', () => {
    const session: AgentSession = {
      ...idleSession(),
      status: 'working',
      turns: [
        ...idleSession().turns,
        {
          id: 'provider-turn',
          turn_count: 2,
          status: 'running',
          provider_turn_started: true,
          provider_resume_at: null,
          started_at: 150,
          completed_at: null,
          checkpoint: null,
        },
      ],
    }
    const result = reduceRuntimeEvent(session, event('promptSubmitted', SUBMISSION), clock)

    expect(result.session.turns).toHaveLength(2)
    expect(result.session.messages.at(-1)).toMatchObject({
      id: SUBMISSION.messageId,
      turn_id: 'provider-turn',
      role: 'user',
      content: 'Second prompt',
    })
  })

  test('names an unnamed task after its first submitted prompt', () => {
    const session: AgentSession = { ...idleSession(), messages: [], turns: [] }
    const result = reduceRuntimeEvent(
      session,
      event('promptSubmitted', { ...SUBMISSION, message: 'Add a dark mode toggle to settings' }),
      clock,
    )

    expect(result.session.auto_title).toBe('Add a dark mode toggle to settings')
    expect(result.session.turns.at(-1)?.turn_count).toBe(1)
  })
})

describe('native issue review', () => {
  test('explicit lifecycle stores the full result and a bounded summary under the current turn', () => {
    let session = reviewSession()
    session.review!.status = 'draft'
    session = apply(session, 'reviewStarted', null)
    expect(session.review?.status).toBe('running')
    session = apply(session, 'turnStarted', null)
    session = apply(session, 'textDelta', 'Checking the implementation…')
    session = apply(session, 'activity', { id: 'read', title: 'Read files', complete: false })
    const text = `### Findings\n\n${'Detailed 🐈 findings '.repeat(100)}`
    const result = reduceRuntimeEvent(session, event('reviewCompleted', { review: text }), clock)
    expect(result.settled).toBe(false)
    expect(result.session.turns.at(-1)?.status).toBe('running')
    expect(result.session.review?.status).toBe('completed')
    const message = result.session.messages.at(-1)!
    expect(message).toMatchObject({ turn_id: 'turn', role: 'assistant', content: text, streaming: false })
    expect(result.session.review?.result_message_id).toBe(message.id)
    expect(result.session.review?.summary).toBe(
      [...text].slice(0, 400).join('').trim().split(/\s+/u).join(' '),
    )
    expect([...result.session.review!.summary!].length).toBeLessThanOrEqual(400)
    expect(result.session.messages.every((message) => !message.streaming)).toBe(true)
    const activities = result.session.transcript_blocks[0]!.content
    if (activities.kind !== 'activities') throw new Error('Expected activities')
    expect(activities.data.every((activity) => activity.complete)).toBe(true)
    const settled = apply(result.session, 'turnFinished', { success: true })
    expect(settled.review?.status).toBe('completed')
    expect(settled.status).toBe('idle')
    expect(settled.messages).toEqual(result.session.messages)
  })

  test('summary retains the words inside the first four hundred characters', () => {
    const session = apply(reviewSession(), 'reviewCompleted', { review: 'x '.repeat(100) })
    expect(session.review?.summary).toBe(Array(100).fill('x').join(' '))
    expect(session.messages.at(-1)?.content).toBe('x '.repeat(100))
  })

  test('large single tokens produce a bounded summary without splitting Unicode characters', () => {
    const text = '🐈'.repeat(100_000)
    const session = apply(reviewSession(), 'reviewCompleted', { review: text })
    expect(session.review?.summary).toBe('🐈'.repeat(400))
    expect(session.messages.at(-1)?.content).toBe(text)
  })

  test('duplicate lifecycle and submission echoes preserve the same result exactly once', () => {
    let session = apply(reviewSession(), 'reviewCompleted', { review: 'No findings.' })
    const recorded = JSON.parse(JSON.stringify(session)) as AgentSession
    session = apply(session, 'reviewStarted', null)
    session = apply(session, 'promptSubmitted', { message: 'Go', turnId: 'turn', messageId: 'message' })
    session = apply(session, 'reviewCompleted', { review: 'No findings.' })
    expect(session.review).toEqual(recorded.review)
    expect(session.messages).toEqual(recorded.messages)
    session = apply(session, 'turnFinished', { success: true })
    const settled = session
    session = apply(session, 'reviewCompleted', { review: 'No findings.' })
    expect(session.review).toEqual(settled.review)
    expect(session.messages).toEqual(settled.messages)
  })

  test('a new submitted review clears only the previous result and fix link', () => {
    const previous: AgentSession = {
      ...idleSession(),
      review: {
        target: { kind: 'baseBranch', branch: 'main' },
        source_session_id: 'development',
        status: 'completed',
        summary: 'Old findings',
        result_message_id: 'reply',
        fix_session_id: 'fix-conversation',
      },
    }
    const session = apply(previous, 'promptSubmitted', SUBMISSION)
    expect(session.review).toEqual({
      target: { kind: 'baseBranch', branch: 'main' },
      source_session_id: 'development',
      status: 'running',
      summary: null,
      result_message_id: null,
      fix_session_id: null,
    })
    expect(session.turns.at(-1)?.id).toBe(SUBMISSION.turnId)
    expect(previous.review?.status).toBe('completed')
  })

  test('ordinary assistant text does not imply a review result', () => {
    const session = apply(reviewSession(), 'textDelta', 'Review complete, no issues found.')
    const result = apply(session, 'turnFinished', { success: true })
    expect(result.review?.status).toBe('failed')
    expect(result.review?.result_message_id).toBeFalsy()
    expect(result.review?.summary).toBeFalsy()
    expect(result.messages.at(-1)?.content).toBe('Review complete, no issues found.')
  })

  test('blank and malformed review results never complete a review', () => {
    for (const payload of [null, {}, { review: 5 }, { review: ' \n ' }]) {
      const session = apply(reviewSession(), 'reviewCompleted', payload)
      expect(session.messages).toEqual(reviewSession().messages)
      expect(session.review?.result_message_id).toBeFalsy()
      expect(apply(session, 'turnFinished', { success: true }).review?.status).toBe('failed')
    }
  })

  test('interruption wins over a success flag and leaves the session idle', () => {
    for (const success of [true, false]) {
      const result = reduceRuntimeEvent(
        reviewSession(), event('turnFinished', { success, interrupted: true }), clock,
      )
      expect(result.settled).toBe(true)
      expect(result.session.review?.status).toBe('interrupted')
      expect(result.session.turns.at(-1)?.status).toBe('interrupted')
      expect(result.session.status).toBe('idle')
    }
  })

  test('failed settlement cannot promote a supplied review result to success', () => {
    const session = apply(reviewSession(), 'reviewCompleted', { review: 'A finding' })
    const failed = apply(session, 'turnFinished', { success: false })
    expect(failed.review?.status).toBe('failed')
    expect(failed.review?.result_message_id).toBe(session.review?.result_message_id)
    expect(failed.messages.at(-1)?.content).toBe('A finding')
  })

  test('startup refusal and process exit fail unsettled review metadata', () => {
    const error = reduceRuntimeEvent(reviewSession(), event('error', 'Native review unavailable'), clock)
    const refused = reduceRuntimeEvent(error.session, event('turnFinished', { success: false, summary: 'Native review unavailable' }), clock)
    expect(error.error).toBe('Native review unavailable')
    expect(refused.session.review?.status).toBe('failed')
    expect(refused.session.status).toBe('failed')
    const failure = apply(refused.session, 'turnFinished', { success: false })
    expect(failure.review?.status).toBe('failed')
    const working = apply(reviewSession(), 'turnStarted', null)
    const exited = reduceRuntimeEvent(working, event('processExited', null), clock)
    expect(exited.removeRuntime).toBe(true)
    expect(exited.settled).toBe(true)
    expect(exited.session.review?.status).toBe('failed')
    expect(apply(working, 'error', 'Transport retry pending').review?.status).toBe('running')
  })

  test('exit after receiving a result but before settlement fails that review', () => {
    const session = apply(reviewSession(), 'reviewCompleted', { review: 'A finding' })
    expect(apply(session, 'processExited', null).review?.status).toBe('failed')
  })

  test('late ordinary events preserve an already settled review', () => {
    const completed = apply(
      apply(reviewSession(), 'reviewCompleted', { review: 'No findings' }),
      'turnFinished', { success: true },
    )
    for (const [kind, payload] of [
      ['error', 'Later transport issue'],
      ['processExited', null],
      ['turnFinished', { success: false }],
      ['reviewStarted', null],
      ['textDelta', 'Late text'],
    ] as const) {
      const session = apply(completed, kind, payload)
      expect(session.review).toEqual(completed.review)
      expect(session.messages).toEqual(completed.messages)
    }
  })

  test('lifecycle events do not turn ordinary conversations into reviews', () => {
    let session = apply(runningSession(), 'reviewStarted', null)
    session = apply(session, 'reviewCompleted', { review: 'Unexpected review' })
    expect(session.review).toBeUndefined()
    expect(session.messages).toEqual(runningSession().messages)
  })
})

test('retryable errors preserve successful native review output', () => {
  let session = apply(reviewSession(), 'turnStarted', null)
  session = apply(session, 'reviewStarted', null)
  const retried = reduceRuntimeEvent(session, event('error', 'Retrying transient provider error'), clock)
  expect(retried.error).toBe('Retrying transient provider error')
  expect(retried.session.review?.status).toBe('running')
  session = apply(retried.session, 'reviewCompleted', { review: '[P1] Preserve carts' })
  session = apply(session, 'turnFinished', { success: true })
  expect(session.review?.status).toBe('completed')
  expect(session.messages.at(-1)?.content).toBe('[P1] Preserve carts')
})

test('review mutations advance a future catalog revision monotonically', () => {
  const before = { ...reviewSession(), updated_at: 500 }
  const result = apply(before, 'reviewCompleted', { review: 'No findings' })
  expect(result.updated_at).toBe(501)
})

describe('provider-initiated Pi turns', () => {
  for (const provider of ['pi', 'ohMyPi'] as const) {
    for (const status of ['idle', 'background'] as const) {
      test(`${provider} streams and settles a wake from ${status}`, () => {
        const previous: AgentSession = { ...idleSession(), provider, status }
        let session = apply(previous, 'turnStarted', null)

        expect(session.status).toBe('working')
        expect(session.turns).toHaveLength(2)
        const turn = session.turns.at(-1)!
        expect(turn).toMatchObject({
          turn_count: 2,
          status: 'running',
          provider_turn_started: true,
        })
        expect(session.messages).toEqual(previous.messages)

        // Replayed starts confirm the same turn instead of opening another.
        session = apply(session, 'turnStarted', null)
        expect(session.turns).toHaveLength(2)
        session = apply(session, 'activity', {
          id: 'wake-tool', kind: 'tool', title: 'Read child result', complete: true,
        })
        session = apply(session, 'textDelta', 'The child finished.')
        expect(session.messages.at(-1)).toMatchObject({
          turn_id: turn.id, role: 'assistant', content: 'The child finished.', streaming: true,
        })
        expect(session.transcript_blocks.at(-1)?.turn_id).toBe(turn.id)
        const activity = session.transcript_blocks.at(-1)?.content
        expect(activity?.kind).toBe('activities')
        if (activity?.kind === 'activities') {
          expect(activity.data[0]?.title).toBe('Read child result')
        }

        const result = reduceRuntimeEvent(session, event('turnFinished', { success: true }), clock)
        expect(result.settled).toBe(true)
        expect(result.session.status).toBe('idle')
        expect(result.session.turns.at(-1)).toMatchObject({ id: turn.id, status: 'completed' })
        expect(result.session.messages.at(-1)?.streaming).toBe(false)
        expect(result.session.messages.filter((message) => message.role === 'user')).toHaveLength(1)
        expect(previous.turns).toHaveLength(1)
        expect(previous.messages).toHaveLength(2)
      })
    }

    test(`${provider} confirms an existing prompted turn`, () => {
      const previous: AgentSession = { ...runningSession(), provider }
      const session = apply(previous, 'turnStarted', null)
      expect(session.turns).toHaveLength(1)
      expect(session.turns[0]).toMatchObject({ id: 'turn', provider_turn_started: true })
      expect(session.messages).toEqual(previous.messages)
      expect(session.status).toBe('working')
    })

    test(`${provider} does not invent a turn from an extension message or output alone`, () => {
      const previous: AgentSession = { ...idleSession(), provider }
      let session = apply(previous, 'extensionMessage', {
        customType: 'notification', text: 'Child finished', display: true,
      })
      session = apply(session, 'textDelta', 'No run has started')
      expect(session.turns).toEqual(previous.turns)
      expect(session.messages).toEqual(previous.messages)
      expect(session.status).toBe('idle')
    })
  }
})

test('MCP tool identity survives partial updates and stays separate in expanded details', () => {
  const call = {
    id: 'tool-1', source_id: 'call-1', kind: 'tool' as const,
    title: 'List running apps via CUA', detail: null,
    tool_name: 'js', mcp_server: 'waku_js_repl', arguments: '{}',
    failed: false, complete: false,
  }
  const started = apply(runningSession(), 'richActivity', call)
  const completed = apply(started, 'richActivity', {
    ...call, id: 'update-1', tool_name: null, mcp_server: null, arguments: null,
    output: '2 apps', complete: true,
  })
  const content = completed.transcript_blocks[0]!.content
  if (content.kind !== 'activities') throw new Error('Expected an activity block')
  const item = content.data[0]!
  expect(item.id).toBe('tool-1')
  expect(item.title).toBe('List running apps via CUA')
  expect(activityDisclosureSections(item)).toEqual([
    { kind: 'mcp-server', label: 'MCP server', content: 'waku_js_repl' },
    { kind: 'tool-name', label: 'Tool', content: 'js' },
    { kind: 'arguments', label: 'Arguments', content: '{}' },
    { kind: 'output', label: 'Output', content: '2 apps' },
  ])
  expect(activityDisclosureSections({
    ...call, mcp_server: null, tool_name: 'read_file', arguments: null, detail: 'Permission denied',
  })).toEqual([
    { kind: 'tool-name', label: 'Tool', content: 'read_file' },
    { kind: 'detail', label: null, content: 'Permission denied' },
  ])
})

function apply(session: AgentSession, kind: string, payload: unknown) {
  return reduceRuntimeEvent(session, event(kind, payload), clock).session
}

function event(kind: string, payload: unknown): SequencedEvent {
  return {
    sessionId: 'session',
    runtimeId: 'runtime',
    epoch: 'epoch',
    sequence: 1,
    event: { kind, payload: payload as never },
  }
}

/** One completed turn, runtime still attached, nothing running. */
function idleSession(): AgentSession {
  return {
    ...runningSession(),
    status: 'idle',
    messages: [
      {
        id: 'message',
        turn_id: 'turn',
        role: 'user',
        content: 'Go',
        created_at: 100,
        streaming: false,
      },
      {
        id: 'reply',
        turn_id: 'turn',
        role: 'assistant',
        content: 'Done',
        created_at: 110,
        streaming: false,
      },
    ],
    turns: [
      {
        id: 'turn',
        turn_count: 1,
        status: 'completed',
        provider_turn_started: true,
        provider_resume_at: null,
        started_at: 100,
        completed_at: 110,
        checkpoint: null,
      },
    ],
  }
}

function runningSession(): AgentSession {
  return {
    id: 'session',
    title: 'New task',
    project_id: 'project',
    workspace: { kind: 'local' },
    provider: 'codex',
    runtime_mode: 'fullAccess',
    status: 'connecting',
    created_at: 100,
    updated_at: 100,
    provider_cursor: null,
    messages: [
      {
        id: 'message',
        turn_id: 'turn',
        role: 'user',
        content: 'Go',
        created_at: 100,
        streaming: false,
      },
    ],
    transcript_blocks: [],
    turns: [
      {
        id: 'turn',
        turn_count: 1,
        status: 'running',
        provider_turn_started: false,
        provider_resume_at: null,
        started_at: 100,
        completed_at: null,
        checkpoint: null,
      },
    ],
  }
}

function reviewSession(): AgentSession {
  return {
    ...runningSession(),
    review: {
      target: { kind: 'uncommittedChanges' },
      source_session_id: 'development',
      status: 'running',
    },
  }
}
