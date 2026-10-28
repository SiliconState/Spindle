// A scripted model for the package's Node tests: OpenAI chat-completions streams
// served by a fake `fetch`, request by request, with what each request carried.

export const KEY = 'sk-embedded-test-key';

export function catalog() {
  return { version: 4, active_provider: 'fake', providers: [{
    id: 'fake', display_name: 'Fake', api_provider: 'openai',
    request_contract: 'openai-chat-completions', base_url: 'https://fake.invalid/v1',
    default_model: 'fake-model', models: ['fake-model', 'small-model'], requires_api_key: true,
    // small-model keeps about 37 KiB of history before a turn compacts it.
    model_specs: {
      'fake-model': { context_window: 200000, max_output_tokens: 4096 },
      'small-model': { context_window: 20000, max_output_tokens: 4096 },
    },
  }] };
}

export const text = (content) => ({ content });
export const call = (name, args) => ({ call: [name, args] });
/** A reply that streams `first` and then waits until the request is aborted. */
export const hold = (first = '') => ({ hold: first });

export function sse(reply, index) {
  const delta = reply.call
    ? { tool_calls: [{ index: 0, id: `call_${index}`, type: 'function', function: { name: reply.call[0], arguments: JSON.stringify(reply.call[1]) } }] }
    : { content: reply.content };
  const finish = reply.call ? 'tool_calls' : 'stop';
  const chunks = [
    { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: null }] },
    { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: finish }] },
    { id: 'c', object: 'chat.completion.chunk', choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } },
  ];
  return chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join('') + 'data: [DONE]\n\n';
}

/** A fake fetch answering from `replies` (in order); `seen` records each request. */
export function scriptedFetch(replies) {
  const seen = [];
  const fetch = async (url, init) => {
    const body = new TextDecoder().decode(init.body);
    seen.push({ url, headers: init.headers, body });
    const reply = fetch.replies.shift() ?? text('(script ended)');
    const encoder = new TextEncoder();
    if (reply.hold !== undefined) {
      const first = sse({ content: reply.hold || 'unfinished' }, seen.length).split('\n\n')[0] + '\n\n';
      const stream = new ReadableStream({
        start(controller) {
          controller.enqueue(encoder.encode(first));
          init.signal?.addEventListener('abort', () => controller.error(new DOMException('aborted', 'AbortError')));
        },
      });
      fetch.holding?.();
      return new Response(stream, { status: 200, headers: { 'content-type': 'text/event-stream' } });
    }
    const bytes = encoder.encode(sse(reply, seen.length));
    // Streamed in small parts, as a network delivers it.
    const stream = new ReadableStream({
      start(controller) {
        for (let at = 0; at < bytes.length; at += 37) controller.enqueue(bytes.subarray(at, at + 37));
        controller.close();
      },
    });
    return new Response(stream, { status: 200, headers: { 'content-type': 'text/event-stream' } });
  };
  fetch.seen = seen;
  fetch.replies = replies;
  return fetch;
}
