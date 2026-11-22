import { Buffer } from 'node:buffer';
import { freemem, totalmem } from 'node:os';
import type { Duplex } from 'node:stream';

import {
  connectNativeTransport, nativeEndpoint, RequestCancelledError, startNativeService, usesWslTransport,
} from './nativeTransport.js';
export {
  defaultExecutablePath, defaultSocketPath, normalizeExecutablePath, normalizeSocketPath,
  RequestCancelledError, usesWslTransport, windowsLocalLaunch,
} from './nativeTransport.js';

import type {
  CancellationLike,
  ClientSettings,
  DaemonRequest,
  DisposableLike,
  JsonValue,
} from './types.js';

const MAX_FRAME_BYTES = 2 * 1024 * 1024;
const FRAMED_JSON_MAGIC = Buffer.from('CCTXJ1\n', 'ascii');
const EMPTY_BUFFER = Buffer.alloc(0);

interface PendingResponse {
  resolve(value: JsonValue): void;
  reject(error: Error): void;
  timer: NodeJS.Timeout;
  cancellation: DisposableLike;
}

export interface ClientTrace {
  [key: string]: JsonValue;
  trace_id: string;
  queue_ms: number;
  prepare_ms: number;
  ipc_ms: number;
  serialize_ms: number;
  write_ms: number;
  total_ms: number;
  started_unix_ms: number;
  completed_unix_ms: number;
  success: boolean;
  available: boolean;
}

export class DaemonResponseError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'DaemonResponseError';
  }
}

export class CacheMissError extends DaemonResponseError {
  public constructor(message: string) {
    super(message);
    this.name = 'CacheMissError';
  }
}

export class TracedRequestError extends Error {
  public constructor(public readonly trace: ClientTrace, cause: unknown) {
    super(cause instanceof Error ? cause.message : String(cause), { cause });
    this.name = 'TracedRequestError';
  }
}

export class DaemonClient {
  private socket: Duplex | undefined;
  private socketPath: string | undefined;
  private receiveBuffer: Buffer = EMPTY_BUFFER;
  private receiveChunks: Buffer[] = [];
  private receiveBytes = 0;
  private pending: PendingResponse | undefined;
  private queue: Promise<void> = Promise.resolve();
  private disposed = false;
  private tracingAvailable: boolean | undefined;
  private traceOutcomesAvailable = false;
  private hostMemoryReportingAvailable = false;
  private callerSessionAvailable = false;
  private startingService: Promise<void> | undefined;
  private lastServiceStartAt = 0;
  private pendingWslTraces: Array<Record<string, JsonValue>> = [];
  private lastHostMemorySentAt = 0;
  private hostMemoryTimer: NodeJS.Timeout | undefined;
  private hostMemorySeq = 0;
  private barrierLane: DaemonClient | undefined;
  private usageLane: DaemonClient | undefined;

  public constructor(
    private readonly settings: () => ClientSettings,
    private readonly serviceStarter: (settings: ClientSettings) => Promise<void> = startNativeService,
    private readonly connector = connectNativeTransport,
    private readonly isBarrierLane = false,
    private readonly onCapabilities?: (caps: {
      tracing: boolean;
      outcomes: boolean;
      hostMemory: boolean;
      callerSession: boolean;
    }) => void,
  ) {}

  private getBarrierLane(): DaemonClient {
    this.barrierLane ??= new DaemonClient(
      this.settings,
      this.serviceStarter,
      this.connector,
      true,
      (caps) => {
        if (this.tracingAvailable === undefined) {
          this.tracingAvailable = caps.tracing;
          this.traceOutcomesAvailable = caps.outcomes;
          this.hostMemoryReportingAvailable = caps.hostMemory;
          this.callerSessionAvailable = caps.callerSession;
        }
      },
    );
    return this.barrierLane;
  }

  private getUsageLane(): DaemonClient {
    this.usageLane ??= new DaemonClient(
      this.settings,
      this.serviceStarter,
      this.connector,
      true,
      (caps) => {
        if (this.tracingAvailable === undefined) {
          this.tracingAvailable = caps.tracing;
          this.traceOutcomesAvailable = caps.outcomes;
          this.hostMemoryReportingAvailable = caps.hostMemory;
          this.callerSessionAvailable = caps.callerSession;
        }
      },
    );
    return this.usageLane;
  }

  public get usesWslBridge(): boolean {
    return usesWslTransport(this.settings());
  }

  public get wslDistribution(): string {
    return this.settings().wslDistribution?.trim() ?? '';
  }

  public get endpoint(): string {
    return nativeEndpoint(this.settings());
  }

  public queueTraceForNextRequest(trace: ClientTrace): void {
    if (this.pendingWslTraces.length >= 64) {
      this.pendingWslTraces.shift();
    }
    this.pendingWslTraces.push({ ...trace });
  }

  public queryUsage(
    hours = 24,
    force = false,
    cancellation: CancellationLike = {
      isCancellationRequested: false,
      onCancellationRequested: () => ({ dispose(): void {} }),
    },
  ): Promise<JsonValue> {
    return this.getUsageLane().request(
      {
        root: null,
        name: 'code_usage',
        arguments: force ? { hours, force: true } : { hours },
        compact: true,
      },
      cancellation,
    );
  }

  public request<T = JsonValue>(
    request: DaemonRequest,
    cancellation: CancellationLike,
  ): Promise<T> {
    if (request.name === 'code_trace' && this.usesWslBridge) {
      const needsImmediateHostMemory =
        isRecord(request.arguments) &&
        isRecord(request.arguments.host_memory) &&
        (this.lastHostMemorySentAt === 0 || Date.now() - this.lastHostMemorySentAt >= 240_000);
      if (!needsImmediateHostMemory) {
        this.queueTraceForNextRequest(request.arguments as unknown as ClientTrace);
        return Promise.resolve({ recorded: true } as unknown as T);
      }
    }
    if (!this.isBarrierLane && request.name === 'code_sync') {
      return this.getBarrierLane().request<T>(request, cancellation);
    }
    if (!this.isBarrierLane && request.name === 'code_usage') {
      return this.getUsageLane().request<T>(request, cancellation);
    }
    return this.enqueue<T>(request, cancellation);
  }

  public async requestTraced<T = JsonValue>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    traceId: string,
    prepareMs = 0,
    callerId?: number,
  ): Promise<{ value: T; trace: ClientTrace }> {
    if (!this.isBarrierLane && request.name === 'code_sync') {
      return this.getBarrierLane().requestTraced<T>(
        request,
        cancellation,
        traceId,
        prepareMs,
        callerId,
      );
    }
    const started = performance.now();
    const trace: ClientTrace = {
      trace_id: traceId, origin: 'native', operation: request.name, root: request.root,
      queue_ms: 0, prepare_ms: prepareMs, ipc_ms: 0, serialize_ms: 0, write_ms: 0,
      total_ms: 0, success: false, available: false, daemon: null,
      started_unix_ms: Date.now(), completed_unix_ms: 0,
    };
    try {
      const value = await this.enqueue<T>(request, cancellation, trace, callerId);
      trace.total_ms = performance.now() - started + prepareMs;
      trace.success = true;
      trace.completed_unix_ms = Date.now();
      return { value, trace };
    } catch (error: unknown) {
      trace.total_ms = performance.now() - started + prepareMs;
      trace.completed_unix_ms = Date.now();
      throw new TracedRequestError(trace, error);
    }
  }

  private enqueue<T>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    trace?: ClientTrace,
    callerId?: number,
  ): Promise<T> {
    const queuedAt = performance.now();
    const execute = (): Promise<T> => {
      if (trace) {
        trace.queue_ms = performance.now() - queuedAt;
      }
      return this.requestWithRetry<T>(request, cancellation, trace, callerId);
    };
    const run = this.queue.then(execute, execute);
    this.queue = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  public dispose(): void {
    this.disposed = true;
    this.barrierLane?.dispose();
    this.barrierLane = undefined;
    this.usageLane?.dispose();
    this.usageLane = undefined;
    this.disconnect(new Error('Code Context native client was disposed.'));
  }

  public reset(): void {
    if (!this.disposed) {
      this.barrierLane?.reset();
      this.usageLane?.reset();
      this.disconnect(new Error('Code Context native connection was reset.'));
    }
  }

  private async requestWithRetry<T>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    trace?: ClientTrace,
    callerId?: number,
  ): Promise<T> {
    if (this.disposed) {
      throw new Error('Code Context native client is disposed.');
    }
    if (cancellation.isCancellationRequested) {
      throw new RequestCancelledError();
    }
    let firstError: Error | undefined;
    const maxAttempts = this.settings().autoStartService ? 3 : 2;
    for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
      try {
        const preparing = performance.now();
        await this.ensureConnected(cancellation);
        if ((trace || request.name === 'code_trace') && this.tracingAvailable === undefined) {
          const ping = await this.send({
            root: null, name: 'ping', arguments: {}, compact: false,
          }, cancellation);
          this.tracingAvailable = isRecord(ping) && isRecord(ping.capabilities)
            && ping.capabilities.request_tracing === true
            && ping.capabilities.trace_clock === 'unix_ms';
          this.traceOutcomesAvailable = isRecord(ping) && isRecord(ping.capabilities)
            && ping.capabilities.client_trace_outcomes === true;
          this.hostMemoryReportingAvailable = isRecord(ping) && isRecord(ping.capabilities)
            && ping.capabilities.host_memory_reporting === true;
          this.callerSessionAvailable = isRecord(ping) && isRecord(ping.capabilities)
            && ping.capabilities.caller_session_token === true;
          this.onCapabilities?.({
            tracing: this.tracingAvailable,
            outcomes: this.traceOutcomesAvailable,
            hostMemory: this.hostMemoryReportingAvailable,
            callerSession: this.callerSessionAvailable,
          });
        }
        if (trace) {
          trace.prepare_ms += performance.now() - preparing;
          trace.available = this.tracingAvailable === true;
          if (process.platform === 'win32' && this.hostMemoryReportingAvailable) {
            trace.host_memory = {
              platform: 'win32', total_bytes: totalmem(), available_bytes: freemem(),
            };
          } else {
            delete trace.host_memory;
          }
        }
        const hasPiggybackTraces =
          this.pendingWslTraces.length > 0 &&
          request.name !== 'code_trace' &&
          request.name !== 'ping';
        const includeCallerToken =
          typeof callerId === 'number' &&
          callerId > 0 &&
          this.callerSessionAvailable &&
          (request.name === 'code_files' ||
            request.name === 'code_grep' ||
            request.name === 'code_context' ||
            request.name === 'code_read');
        const needsArgMutation =
          request.name === 'code_trace' ||
          Boolean(trace && this.tracingAvailable) ||
          hasPiggybackTraces ||
          includeCallerToken;
        const argumentsForDaemon = needsArgMutation ? { ...request.arguments } : request.arguments;
        if (includeCallerToken) {
          argumentsForDaemon._cctx_caller = callerId;
        }
        let sentHostMemory = false;
        if (request.name === 'code_trace') {
          if (!this.traceOutcomesAvailable) { delete argumentsForDaemon.outcome; }
          if (!this.hostMemoryReportingAvailable) { delete argumentsForDaemon.host_memory; }
          sentHostMemory = isRecord(argumentsForDaemon.host_memory);
        }
        if (hasPiggybackTraces) {
          let latestPiggybackMemIdx = -1;
          const drained = this.pendingWslTraces.splice(0, this.pendingWslTraces.length).map((item, idx) => {
            const copy = { ...item };
            if (!this.traceOutcomesAvailable) { delete copy.outcome; }
            if (!this.hostMemoryReportingAvailable) { delete copy.host_memory; }
            if (isRecord(copy.host_memory)) {
              const sampleAt = typeof copy.completed_unix_ms === 'number' ? copy.completed_unix_ms : 0;
              if (sampleAt > 0 && sampleAt < this.lastHostMemorySentAt) {
                delete copy.host_memory;
              } else {
                latestPiggybackMemIdx = idx;
              }
            }
            return copy;
          });
          if (latestPiggybackMemIdx >= 0) {
            for (let i = 0; i < drained.length; i += 1) {
              if (i !== latestPiggybackMemIdx) {
                delete drained[i]?.host_memory;
              }
            }
            if (process.platform === 'win32' && this.hostMemoryReportingAvailable) {
              drained[latestPiggybackMemIdx]!.host_memory = {
                platform: 'win32',
                total_bytes: totalmem(),
                available_bytes: freemem(),
              };
            }
            sentHostMemory = true;
          }
          argumentsForDaemon._cctx_prev_trace = drained;
        }
        if (trace && this.tracingAvailable) {
          argumentsForDaemon._cctx_trace = trace.trace_id;
        }
        const outgoing = needsArgMutation
          ? { ...request, arguments: argumentsForDaemon }
          : request;
        const ipcStarted = performance.now();
        let value: JsonValue;
        try {
          value = await this.send(outgoing, cancellation);
        } finally {
          if (trace) {
            trace.ipc_ms += performance.now() - ipcStarted;
          }
        }
        if (sentHostMemory) {
          this.lastHostMemorySentAt = Date.now();
        }
        this.ensureHostMemoryKeepalive();
        if (trace && this.tracingAvailable) {
          if (!isRecord(value) || !isRecord(value._cctx_trace)
              || value._cctx_trace.trace_id !== trace.trace_id || value.value === undefined) {
            throw new DaemonResponseError('Daemon trace is missing or does not match the request.');
          }
          trace.daemon = value._cctx_trace;
          return value.value as T;
        }
        return value as T;
      } catch (error: unknown) {
        const failure = asError(error);
        if (cancellation.isCancellationRequested) {
          throw new RequestCancelledError();
        }
        if (
          this.disposed ||
          failure instanceof RequestCancelledError ||
          failure instanceof DaemonResponseError
        ) {
          throw failure;
        }
        firstError ??= failure;
        this.disconnect(failure);
        if (attempt === 0 && this.settings().autoStartService) {
          await this.startServiceCoalesced();
          await delay(80 + (process.pid % 40), cancellation);
        } else if (attempt === 1 && maxAttempts === 3) {
          await delay(120, cancellation);
        }
      }
    }
    throw new Error(
      `Code Context daemon request failed after reconnect: ${firstError?.message ?? 'unknown error'}`,
    );
  }

  private async startServiceCoalesced(): Promise<void> {
    if (
      this.disposed ||
      (Date.now() - this.lastServiceStartAt < 2_000 && !this.startingService)
    ) {
      return;
    }
    this.startingService ??= this.serviceStarter(this.settings()).finally(() => {
      this.lastServiceStartAt = Date.now();
      this.startingService = undefined;
    });
    await this.startingService;
  }

  private async ensureConnected(cancellation: CancellationLike): Promise<void> {
    const settings = this.settings();
    const expectedPath = nativeEndpoint(settings);
    if (this.socket && !this.socket.destroyed && this.socketPath === expectedPath) {
      return;
    }
    this.disconnect(new Error('Replacing stale Code Context connection.'));
    const socket = await this.connector(settings, cancellation);
    if (this.disposed || cancellation.isCancellationRequested) {
      socket.destroy();
      if (cancellation.isCancellationRequested) {
        throw new RequestCancelledError();
      }
      throw new Error('Code Context native client was disposed while connecting.');
    }
    this.socket = socket;
    this.socketPath = expectedPath;
    this.receiveBuffer = EMPTY_BUFFER;
    this.receiveChunks = [];
    this.receiveBytes = 0;
    socket.on('data', (chunk: Buffer) => this.onData(socket, chunk));
    socket.on('error', (error: Error) => {
      if (this.socket === socket) {
        this.disconnect(error);
      }
    });
    socket.on('close', () => {
      if (this.socket === socket) {
        this.disconnect(new Error('Code Context daemon connection closed.'));
      }
    });
    await new Promise<void>((resolveConnect, rejectConnect) => {
      socket.write(FRAMED_JSON_MAGIC, (error?: Error | null) => {
        if (error) {
          if (this.socket === socket) {
            this.disconnect(error);
          }
          rejectConnect(error);
        } else {
          resolveConnect();
        }
      });
    });
  }

  private async send(
    request: DaemonRequest,
    cancellation: CancellationLike,
  ): Promise<JsonValue> {
    const socket = this.socket;
    if (!socket || socket.destroyed) {
      throw new Error('Code Context daemon is not connected.');
    }
    if (this.pending) {
      throw new Error('Code Context protocol allows only one in-flight request.');
    }
    const json = JSON.stringify(request);
    const byteLength = Buffer.byteLength(json, 'utf8');
    if (byteLength > MAX_FRAME_BYTES) {
      throw new Error('Code Context native request exceeds 2 MiB.');
    }
    const frame = Buffer.allocUnsafe(byteLength + 4);
    frame.writeUInt32BE(byteLength, 0);
    frame.write(json, 4, byteLength, 'utf8');
    const timeoutMs = this.settings().requestTimeoutMs;
    return new Promise<JsonValue>((resolveResponse, rejectResponse) => {
      const timer = setTimeout(() => {
        this.disconnect(
          new Error(`Code Context daemon request timed out after ${timeoutMs} ms.`),
        );
      }, timeoutMs);
      const cancellationListener = cancellation.onCancellationRequested(() => {
        this.disconnect(new RequestCancelledError());
      });
      this.pending = {
        resolve: resolveResponse,
        reject: rejectResponse,
        timer,
        cancellation: cancellationListener,
      };
      socket.write(frame, (error?: Error | null) => {
        if (error) {
          this.disconnect(error);
        }
      });
    });
  }

  private onData(socket: Duplex, chunk: Buffer): void {
    if (this.socket !== socket || chunk.byteLength === 0) {
      return;
    }
    if (
      this.receiveBuffer.byteLength === 0 &&
      this.receiveChunks.length === 0 &&
      chunk.byteLength >= 4
    ) {
      const length = chunk.readUInt32BE(0);
      if (length > MAX_FRAME_BYTES) {
        this.disconnect(new Error('Code Context daemon response exceeds 2 MiB.'));
        return;
      }
      if (chunk.byteLength === length + 4) {
        const pending = this.takePending();
        if (!pending) {
          this.disconnect(new Error('Code Context daemon sent an unsolicited response.'));
          return;
        }
        try {
          const response = JSON.parse(chunk.toString('utf8', 4, length + 4)) as {
            result?: JsonValue;
            error?: string;
          };
          if (typeof response.error === 'string') {
            pending.reject(
              response.error.startsWith('CACHE_MISS ')
                ? new CacheMissError(response.error)
                : new DaemonResponseError(response.error),
            );
          } else if (response.result !== undefined) {
            pending.resolve(response.result);
          } else {
            pending.reject(new Error('Code Context daemon response has no result.'));
          }
        } catch (error: unknown) {
          pending.reject(asError(error));
          this.disconnect(asError(error));
        }
        return;
      }
    }
    if (this.receiveBuffer.byteLength > 0) {
      this.receiveChunks.push(this.receiveBuffer);
      this.receiveBytes += this.receiveBuffer.byteLength;
      this.receiveBuffer = EMPTY_BUFFER;
    }
    this.receiveChunks.push(chunk);
    this.receiveBytes += chunk.byteLength;
    if (this.receiveBytes > MAX_FRAME_BYTES + 4) {
      this.disconnect(new Error('Code Context daemon response exceeds 2 MiB.'));
      return;
    }
    if (this.receiveBytes < 4) {
      return;
    }
    const firstChunk = this.receiveChunks[0]!;
    if (firstChunk.byteLength < 4) {
      this.receiveBuffer = Buffer.concat(this.receiveChunks, this.receiveBytes);
      this.receiveChunks = [];
      this.receiveBytes = 0;
    }
    const headerBuf = this.receiveBuffer.byteLength > 0 ? this.receiveBuffer : this.receiveChunks[0]!;
    const length = headerBuf.readUInt32BE(0);
    if (length > MAX_FRAME_BYTES) {
      this.disconnect(new Error('Code Context daemon response exceeds 2 MiB.'));
      return;
    }
    const totalAvailable = this.receiveBuffer.byteLength > 0 ? this.receiveBuffer.byteLength : this.receiveBytes;
    if (totalAvailable < length + 4) {
      return;
    }
    const assembled =
      this.receiveBuffer.byteLength > 0
        ? this.receiveBuffer
        : this.receiveChunks.length === 1
          ? this.receiveChunks[0]!
          : Buffer.concat(this.receiveChunks, this.receiveBytes);
    this.receiveChunks = [];
    this.receiveBytes = 0;
    const frame = assembled.subarray(4, length + 4);
    this.receiveBuffer =
      assembled.byteLength === length + 4
        ? EMPTY_BUFFER
        : assembled.subarray(length + 4);
    const pending = this.takePending();
    if (!pending) {
      this.disconnect(new Error('Code Context daemon sent an unsolicited response.'));
      return;
    }
    if (this.receiveBuffer.byteLength > 0) {
      pending.reject(new Error('Code Context daemon sent multiple responses.'));
      this.disconnect(new Error('Code Context protocol desynchronized.'));
      return;
    }
    try {
      const response = JSON.parse(frame.toString('utf8')) as {
        result?: JsonValue;
        error?: string;
      };
      if (typeof response.error === 'string') {
        pending.reject(response.error.startsWith('CACHE_MISS ')
          ? new CacheMissError(response.error) : new DaemonResponseError(response.error));
      } else if (response.result !== undefined) {
        pending.resolve(response.result);
      } else {
        pending.reject(new Error('Code Context daemon response has no result.'));
      }
    } catch (error: unknown) {
      pending.reject(new Error(`Invalid Code Context daemon response: ${asError(error).message}`));
      this.disconnect(asError(error));
    }
  }

  private takePending(): PendingResponse | undefined {
    const pending = this.pending;
    this.pending = undefined;
    if (pending) {
      clearTimeout(pending.timer);
      pending.cancellation.dispose();
    }
    return pending;
  }

  private ensureHostMemoryKeepalive(): void {
    if (
      this.isBarrierLane ||
      this.disposed ||
      process.platform !== 'win32' ||
      !this.usesWslBridge ||
      !this.hostMemoryReportingAvailable ||
      this.hostMemoryTimer !== undefined
    ) {
      return;
    }
    const timer = setInterval(() => {
      if (
        this.disposed ||
        !this.socket ||
        this.socket.destroyed ||
        this.pending ||
        !this.hostMemoryReportingAvailable ||
        Date.now() - this.lastHostMemorySentAt < 240_000
      ) {
        return;
      }
      this.hostMemorySeq += 1;
      const now = Date.now();
      const heartbeat: Record<string, JsonValue> = {
        trace_id: `winmem_${process.pid}_${this.hostMemorySeq}`,
        origin: 'native',
        operation: 'host_memory_heartbeat',
        root: null,
        daemon: null,
        queue_ms: 0,
        prepare_ms: 0,
        ipc_ms: 0,
        serialize_ms: 0,
        write_ms: 0,
        total_ms: 0,
        success: true,
        available: true,
        started_unix_ms: now,
        completed_unix_ms: now,
        host_memory: {
          platform: 'win32',
          total_bytes: totalmem(),
          available_bytes: freemem(),
        },
      };
      if (this.traceOutcomesAvailable) {
        heartbeat.outcome = 'success';
      }
      void this.enqueue(
        { root: null, name: 'code_trace', arguments: heartbeat, compact: true },
        {
          isCancellationRequested: false,
          onCancellationRequested: (): DisposableLike => ({ dispose: (): void => undefined }),
        },
      ).catch(() => undefined);
    }, 240_000);
    timer.unref?.();
    this.hostMemoryTimer = timer;
  }

  private disconnect(error: Error): void {
    if (this.hostMemoryTimer !== undefined) {
      clearInterval(this.hostMemoryTimer);
      this.hostMemoryTimer = undefined;
    }
    const socket = this.socket;
    this.socket = undefined;
    this.socketPath = undefined;
    this.tracingAvailable = undefined;
    this.traceOutcomesAvailable = false;
    this.hostMemoryReportingAvailable = false;
    this.callerSessionAvailable = false;
    this.lastHostMemorySentAt = 0;
    this.receiveBuffer = EMPTY_BUFFER;
    this.receiveChunks = [];
    this.receiveBytes = 0;
    const pending = this.takePending();
    pending?.reject(error);
    if (socket && !socket.destroyed) {
      socket.destroy();
    }
  }
}

function isRecord(value: JsonValue | undefined): value is { [key: string]: JsonValue } {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

async function delay(milliseconds: number, cancellation: CancellationLike): Promise<void> {
  if (cancellation.isCancellationRequested) {
    throw new RequestCancelledError();
  }
  await new Promise<void>((resolveDelay, rejectDelay) => {
    const timer = setTimeout(() => {
      listener.dispose();
      resolveDelay();
    }, milliseconds);
    const listener = cancellation.onCancellationRequested(() => {
      clearTimeout(timer);
      listener.dispose();
      rejectDelay(new RequestCancelledError());
    });
  });
}
