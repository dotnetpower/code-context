import type { ClientTrace } from './daemonClient.js';
import type { JsonValue } from './types.js';
import type { Outcome } from './usage.js';

export type LogLevel = 'info' | 'warn' | 'error';

export function singleLine(value: string): string {
  let hasControl = false;
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 32 || code === 127) {
      hasControl = true;
      break;
    }
  }
  if (!hasControl) {
    return value;
  }
  return Array.from(value, (character) => {
    if (character.charCodeAt(0) >= 32 && character.charCodeAt(0) !== 127) { return character; }
    return character === '\n' ? '\\n' : character === '\r' ? '\\r' : character === '\t' ? '\\t' : '?';
  }).join('');
}

export function outcomeLevel(outcome: Outcome): LogLevel {
  if (outcome === 'system_error') { return 'error'; }
  return outcome === 'policy_denied' || outcome === 'invalid_request' ? 'warn' : 'info';
}

export function formatTrace(trace: ClientTrace, reason?: string): string {
  const milliseconds = (value: number): string => `${value.toFixed(3)}ms`;
  const parts = [
    `${typeof trace.operation === 'string' ? trace.operation : 'unknown operation'} ${typeof trace.outcome === 'string' ? trace.outcome : trace.success ? 'success' : 'system_error'}`,
    `total=${milliseconds(trace.total_ms)}`,
    `prepare=${milliseconds(trace.prepare_ms)}`,
    `queue=${milliseconds(trace.queue_ms)}`,
    `ipc=${milliseconds(trace.ipc_ms)}`,
    `format=${milliseconds(trace.serialize_ms)}`,
  ];
  const daemon = trace.daemon;
  if (typeof daemon === 'object' && daemon !== null && !Array.isArray(daemon)) {
    if (typeof daemon.execute_ms === 'number') {
      parts.push(`daemon=${milliseconds(daemon.execute_ms)}`);
    }
    if (typeof daemon.daemon_request_id === 'number') {
      parts.push(`request=${daemon.daemon_request_id}`);
    }
  }
  parts.push(`root=${singleLine(typeof trace.root === 'string' ? trace.root : '-')}`,
    `trace=${singleLine(trace.trace_id)}`);
  if (reason) { parts.push(`reason=${singleLine(reason)}`); }
  return parts.join(' ');
}

export function formatLogDetails(title: string, value: JsonValue): string {
  const lines = [singleLine(title)];
  let omitted = false;
  const visit = (path: string, entry: JsonValue): void => {
    if (lines.length >= 256) { omitted = true; return; }
    if (Array.isArray(entry)) {
      lines.push(`${path}: ${entry.length} item(s)`);
      entry.forEach((item, index) => { visit(`${path}[${index}]`, item); });
    } else if (typeof entry === 'object' && entry !== null) {
      const fields = Object.entries(entry);
      if (fields.length === 0) { lines.push(`${path}: (empty)`); }
      for (const [key, item] of fields) { visit(path ? `${path}.${singleLine(key)}` : singleLine(key), item); }
    } else {
      lines.push(`${path}: ${entry === null ? 'not available' : singleLine(String(entry))}`);
    }
  };
  visit('', value);
  if (omitted) { lines.push('Additional fields omitted from this log view (256-line limit); use code_status for the complete structured result.'); }
  return lines.join('\n');
}
