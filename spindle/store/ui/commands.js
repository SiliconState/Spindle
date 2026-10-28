// Verb-first thread commands; unmatched input remains an ordinary model message.
// Settings verbs match only their exact short forms ("use glm-5.3 max", "network on",
// "budget $5"), and a model name only when the catalog knows it, so a request such as
// "use the async API" still reaches the model.
const normalize = text => text.trim().replace(/^\//, '');
const LEVELS = {default: 'default', auto: 'default', off: 'off', low: 'low', medium: 'medium', med: 'medium', high: 'high', xhigh: 'xhigh', max: 'max'};
const LEVEL = '(default|auto|off|low|medium|med|high|xhigh|max)';
// `models`: [{provider, id, label}] from the host's catalog.
function known(models, name) {
  const wanted = name.toLowerCase();
  return (models || []).find(m => m.id.toLowerCase() === wanted) || (models || []).find(m => String(m.label || '').toLowerCase() === wanted) || null;
}
export function intent(raw, models = []) {
  // Search only with its slash: "find the bug" is a request for the model.
  const search = String(raw).trim().match(/^\/(?:find|search)\s+(.+)$/i);
  if (search) return {action: 'find', query: search[1].trim()};
  const slashed = String(raw).trim().startsWith('/');
  const text = normalize(raw);
  if (!text) return null;
  const low = text.toLowerCase().replace(/\s+/g, ' ');
  if (low === 'stop' || low === 'stop the run') return {action: 'stop'};
  if (low === 'send now' || low === 'read it now') return {action: 'hurry'};
  if (low === 'continue') return {action: 'continue'};
  if (low === 'retry' || low === 'retry the model') return {action: 'retry'};
  if (['files', 'show files', 'workspace'].includes(low)) return {action: 'files'};
  if (['settings', 'open settings', 'show settings'].includes(low)) return {action: 'settings'};
  if (['host', 'host settings', 'spindle settings'].includes(low)) return {action: 'host'};
  if (['thread settings', 'thread menu'].includes(low)) return {action: 'menu'};
  if (['history', 'revisions', 'show history'].includes(low)) return {action: 'history'};
  if (['delete this thread', 'delete thread'].includes(low)) return {action: 'delete'};
  if (low === 'new thread') return {action: 'new'};
  if (['export transcript', 'export this thread'].includes(low)) return {action: 'export'};
  if (['download workspace', 'download files'].includes(low)) return {action: 'download'};
  if (['model', 'models', 'change model', 'switch model', 'choose model', 'pick a model'].includes(low)) return {action: 'picker'};
  if ((slashed && ['install', 'promote'].includes(low)) || ['promote', 'install the change', 'install this change', 'install spindle', 'install the spindle change'].includes(low)) return {action: 'install'};
  if ((slashed && low === 'update') || ['update from host', 'update from host source'].includes(low)) return {action: 'update'};
  let match = low.match(/^(?:rewind|undo|go back)(?: to)?(?: revision)? r?(\d+)$/)
    || low.match(/^rewind to r(\d+)$/);
  if (match) return {action: 'rewind', revision: Number(match[1])};
  match = text.match(/^(?:rename|call) (?:this )?thread to (.+)$/i) || text.match(/^rename to (.+)$/i);
  if (match) return {action: 'rename', name: match[1].trim().slice(0, 80)};
  match = low.match(new RegExp(`^(?:set )?(?:reasoning|effort|reasoning effort)(?: to)? ${LEVEL}$`));
  if (match) return {action: 'effort', effort: LEVELS[match[1]]};
  match = low.match(/^(?:(allow|enable|disallow|disable|block) (?:command )?network(?: access)?|(?:command )?network(?: access)? (on|off))$/);
  if (match) return {action: 'settings', fields: {network: ['allow', 'enable', 'on'].includes(match[1] || match[2])}};
  match = low.match(/^(?:(enable|disable) (?:the )?(?:rust )?toolchain|(?:rust )?toolchain (on|off))$/);
  if (match) return {action: 'settings', fields: {toolchain: ['enable', 'on'].includes(match[1] || match[2])}};
  match = low.match(/^(?:set )?(?:the )?budget(?: to)? \$?(\d+(?:\.\d{1,2})?)$/);
  if (match) return {action: 'settings', fields: {max_usd: Number(match[1])}};
  if (/^(?:(?:no|clear|remove) (?:the )?budget|budget (?:off|none|unlimited))$/.test(low)) return {action: 'settings', fields: {max_usd: null}};
  match = low.match(/^(?:(?:use )?(digest|model summary) compaction|compaction (digest|model|model summary))$/);
  if (match) return {action: 'settings', fields: {compaction: (match[1] || match[2]) === 'digest' ? 'digest' : 'model'}};
  // "use glm-5.3", "switch to gpt-5.6 high", "/model glm-5.3 max".
  match = text.match(new RegExp(`^(?:(model)|use|switch to)\\s+(\\S+)(?:\\s+(?:at |with )?${LEVEL}(?: effort| reasoning)?)?$`, 'i'));
  if (match) {
    const found = known(models, match[2]);
    const effort = match[3] ? LEVELS[match[3].toLowerCase()] : undefined;
    if (found) return {action: 'model', provider: found.provider, model: found.id, ...(effort ? {effort} : {})};
    if (match[1] && slashed) return {action: 'model', provider: null, model: match[2], ...(effort ? {effort} : {})};
  }
  match = text.match(/^filter(?: threads)?[:\s]+(.+)$/i);
  if (match) return {action: 'filter', query: match[1].trim()};
  return null;
}
