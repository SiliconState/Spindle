// The engine's document. Chrome keeps it open while panels come and go; its worker
// runs every thread. This page only starts the worker and carries page tool calls to
// the background (a worker has no chrome.runtime).
import { uncertain } from './page-tools.mjs';

const worker = new Worker(new URL('engine-worker.mjs', import.meta.url), { type: 'module' });

worker.onmessage = async ({ data }) => {
  if (data?.type === 'page_tool_cancel') {
    chrome.runtime.sendMessage({ type: data.type, call: data.call }).catch(() => {});
    return;
  }
  if (data?.type !== 'page_tool') return;
  let answer;
  try {
    answer = await chrome.runtime.sendMessage({ type: 'page_tool', call: data.call, name: data.name, input: data.input });
  } catch (error) {
    answer = uncertain(data.name, `Chrome's answer to the page tool was lost: ${error.message}`);
  }
  worker.postMessage({ type: 'page_tool_answer', call: data.call, answer: answer ?? uncertain(data.name, 'Chrome gave no answer') });
};
worker.onerror = (event) => {
  // The panel learns of it over the channel (no answer, or the worker's own report).
  console.error('Spindle engine worker failed:', event.message);
};
