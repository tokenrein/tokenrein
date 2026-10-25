/** A fake Tokenrein proxy that records request headers and can answer with proxy errors. */

import { createServer, type IncomingHttpHeaders, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';

export const OPENAI_OK = {
  id: 'chatcmpl-1',
  object: 'chat.completion',
  created: 0,
  model: 'gpt-4o-mini',
  choices: [{ index: 0, message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }],
  usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
};

export const ANTHROPIC_OK = {
  id: 'msg_1',
  type: 'message',
  role: 'assistant',
  model: 'claude-haiku-4-5',
  content: [{ type: 'text', text: 'ok' }],
  stop_reason: 'end_turn',
  stop_sequence: null,
  usage: { input_tokens: 1, output_tokens: 1 },
};

/** The body the real proxy returns, in the shape of the API being called. */
export function proxyError(path: string, type: string, message: string): unknown {
  if (path.endsWith('/messages')) return { type: 'error', error: { type, message } };
  return { error: { message, type, code: type, param: null } };
}

export interface Recorded {
  path: string;
  headers: IncomingHttpHeaders;
  body: any;
}

export class FakeProxy {
  requests: Recorded[] = [];
  delayMs = 0;
  /** Returned for every request while set. */
  error: { status: number; type: string; message: string } | undefined;
  url = '';
  #server: Server;

  constructor() {
    this.#server = createServer((req, res) => {
      const chunks: Buffer[] = [];
      req.on('data', (c: Buffer) => chunks.push(c));
      req.on('end', () => {
        const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : {};
        const path = req.url ?? '';
        this.requests.push({ path, headers: req.headers, body });
        const runId = (req.headers['x-tr-run-id'] as string | undefined) ?? 'req_generated';
        const reply = () => {
          const status = this.error ? this.error.status : 200;
          const payload = this.error
            ? proxyError(path, this.error.type, this.error.message)
            : path.endsWith('/messages')
              ? ANTHROPIC_OK
              : OPENAI_OK;
          res.writeHead(status, {
            'content-type': 'application/json',
            'x-tr-run-id': runId,
            'retry-after-ms': '1',
          });
          res.end(JSON.stringify(payload));
        };
        if (this.delayMs) setTimeout(reply, this.delayMs);
        else reply();
      });
    });
  }

  async start(): Promise<this> {
    await new Promise<void>((resolve) => this.#server.listen(0, '127.0.0.1', resolve));
    this.url = `http://127.0.0.1:${(this.#server.address() as AddressInfo).port}`;
    return this;
  }

  async close(): Promise<void> {
    this.#server.closeAllConnections();
    await new Promise<void>((resolve) => this.#server.close(() => resolve()));
  }

  reset(): void {
    this.requests = [];
    this.delayMs = 0;
    this.error = undefined;
  }

  last(): Recorded {
    const r = this.requests.at(-1);
    if (!r) throw new Error('no requests recorded');
    return r;
  }
}
