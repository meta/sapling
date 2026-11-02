/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* eslint-disable @typescript-eslint/no-explicit-any */
/* eslint-disable @typescript-eslint/no-unused-vars */
/* eslint-disable @typescript-eslint/no-this-alias */

import type {Writable} from 'shared/typeUtils';
import type {LocalWebSocketEventBus as LocalWebSocketEventBusType} from '../LocalWebSocketEventBus';
import type {PlatformName} from '../types';

import {logger} from '../logger';
import {serializeToString} from '../serialize';

const HEARTBEAT = serializeToString({type: 'heartbeat', id: 'isl-connection'});

const LocalWebSocketEventBus =
  // eslint-disable-next-line @typescript-eslint/consistent-type-imports
  (jest.requireActual('../LocalWebSocketEventBus') as typeof import('../LocalWebSocketEventBus'))
    .LocalWebSocketEventBus;

let globalMockWs: MockWebSocketImpl;
class MockWebSocketImpl extends EventTarget implements WebSocket {
  constructor(
    public url: string,
    _protocols?: string | string[] | undefined,
  ) {
    super();
    globalMockWs = this as unknown as MockWebSocketImpl; // keep track of each new instance as a global to use in tests
  }

  binaryType = 'blob' as const;
  bufferedAmount = 0;
  extensions = '';

  onclose = null;
  onerror = null;
  onmessage = null;
  onopen = null;
  protocol = '';
  readyState: WebSocket['readyState'] = 0;

  readonly OPEN = 1 as const;
  readonly CONNECTING = 0 as const;
  readonly CLOSED = 3 as const;
  readonly CLOSING = 2 as const;

  send(data: string | ArrayBufferLike | Blob | ArrayBufferView): void {
    this.sentMessages.push(data as string);
  }

  // eslint-disable-next-line @typescript-eslint/no-empty-function
  close(code = 1000, reason = ''): void {
    this.simulateServerDisconnected(code, reason);
  }

  // -------- Additional APIs for testing --------

  simulateIncomingMessage(message: string) {
    const e = new Event('message');
    (e as Writable<MessageEvent<string>>).data = message;
    this.dispatchEvent(e);
  }
  simulateTransportConnected() {
    this.dispatchEvent(new Event('open'));
  }
  simulateServerConnected() {
    this.simulateTransportConnected();
    this.simulateIncomingMessage(HEARTBEAT);
  }
  simulateServerDisconnected(code = 1006, reason = '') {
    this.dispatchEvent(new CloseEvent('close', {code, reason}));
  }

  public sentMessages: Array<string> = [];
}
const MockWebSocket = MockWebSocketImpl as unknown as typeof WebSocket;

const DEFAULT_HOST = 'localhost:8080';

function createMessageBus(): LocalWebSocketEventBusType {
  return new LocalWebSocketEventBus(DEFAULT_HOST, MockWebSocket, {
    token: '1234',
    platformName: 'test' as string as PlatformName,
  });
}

describe('LocalWebsocketEventBus', () => {
  it('does not log the open event containing the credentialed socket URL', () => {
    const bus = createMessageBus();
    globalMockWs.simulateServerConnected();

    expect(new URL(globalMockWs.url).searchParams.get('token')).toBe('1234');
    expect(logger.info).toHaveBeenCalledWith('websocket open');
    bus.dispose();
  });

  it('opens and sends messages', () => {
    const bus = createMessageBus();
    globalMockWs.simulateServerConnected();
    bus.postMessage('my message');
    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, 'my message']);
  });

  it('queues messages while connecting', () => {
    const bus = createMessageBus();
    bus.postMessage('first');
    bus.postMessage('second');
    expect(globalMockWs.sentMessages).toEqual([]);
    globalMockWs.simulateServerConnected();
    bus.postMessage('third');
    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, 'first', 'second', 'third']);
  });

  it('handles incoming messages', () => {
    const bus = createMessageBus();
    const onMessage1 = jest.fn();
    const onMessage2 = jest.fn();
    bus.onMessage(onMessage1);
    bus.onMessage(onMessage2);

    globalMockWs.simulateServerConnected();
    globalMockWs.simulateIncomingMessage('incoming message');

    expect(onMessage1).toHaveBeenCalledWith(expect.objectContaining({data: 'incoming message'}));
    expect(onMessage2).toHaveBeenCalledWith(expect.objectContaining({data: 'incoming message'}));
  });

  it('notifies about status', () => {
    const bus = createMessageBus();
    const changeStatus = jest.fn();
    bus.onChangeStatus(changeStatus);

    expect(changeStatus).toHaveBeenCalledWith({type: 'initializing'});
    globalMockWs.simulateServerConnected();

    expect(changeStatus).toHaveBeenCalledWith({type: 'open'});
    expect(changeStatus).toHaveBeenCalledTimes(2);
    changeStatus.mockClear();

    globalMockWs.simulateServerDisconnected();
    expect(changeStatus).toHaveBeenCalledWith({type: 'reconnecting'});
    expect(changeStatus).not.toHaveBeenCalledWith({type: 'open'});

    globalMockWs.simulateServerConnected();
    expect(changeStatus).toHaveBeenCalledWith({type: 'open'});
  });

  it('disposes status handlers properly', () => {
    const bus = createMessageBus();

    const changeStatus1 = jest.fn();
    bus.onChangeStatus(changeStatus1);

    const changeStatus2 = jest.fn();
    const disposable2 = bus.onChangeStatus(changeStatus2);

    const changeStatus3 = jest.fn();
    bus.onChangeStatus(changeStatus3);

    expect(changeStatus1).toHaveBeenCalledWith({type: 'initializing'});
    expect(changeStatus2).toHaveBeenCalledWith({type: 'initializing'});
    expect(changeStatus3).toHaveBeenCalledWith({type: 'initializing'});

    disposable2.dispose();

    globalMockWs.simulateServerConnected();

    expect(changeStatus1).toHaveBeenCalledWith({type: 'open'});
    expect(changeStatus2).not.toHaveBeenCalledWith({type: 'open'});
    expect(changeStatus3).toHaveBeenCalledWith({type: 'open'});
  });

  it('queues up messages while disconnected', () => {
    const bus = createMessageBus();
    globalMockWs.simulateServerConnected();

    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT]);

    globalMockWs.simulateServerDisconnected();

    bus.postMessage('hi');
    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT]);
    globalMockWs.simulateServerConnected();
    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, HEARTBEAT, 'hi']);
  });

  it('previous onMessage handlers exist after reconnection', () => {
    const bus = createMessageBus();

    const onMessage = jest.fn();
    bus.onMessage(onMessage);

    globalMockWs.simulateServerConnected();

    globalMockWs.simulateIncomingMessage('one');
    expect(onMessage).toHaveBeenCalledWith(expect.objectContaining({data: 'one'}));

    globalMockWs.simulateServerDisconnected();
    globalMockWs.simulateServerConnected();

    globalMockWs.simulateIncomingMessage('two');
    expect(onMessage).toHaveBeenCalledWith(expect.objectContaining({data: 'two'}));
  });

  it('clears queued messages after sending them', () => {
    const bus = createMessageBus();
    globalMockWs.simulateServerConnected();

    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT]);

    globalMockWs.simulateServerDisconnected();

    bus.postMessage('hi');
    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT]);
    globalMockWs.simulateServerConnected();
    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, HEARTBEAT, 'hi']);

    globalMockWs.simulateServerDisconnected();
    globalMockWs.simulateServerConnected();

    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, HEARTBEAT, 'hi', HEARTBEAT]);
  });

  it('disposes handlers properly', () => {
    const bus = createMessageBus();
    globalMockWs.simulateServerConnected();

    const onMessage = jest.fn();
    const disposable = bus.onMessage(onMessage);

    globalMockWs.simulateServerConnected();
    globalMockWs.simulateIncomingMessage('incoming message');

    expect(onMessage).toHaveBeenCalledWith(expect.objectContaining({data: 'incoming message'}));
    disposable.dispose();
    globalMockWs.simulateIncomingMessage('another after dispose');
    expect(onMessage).not.toHaveBeenCalledWith(
      expect.objectContaining({data: 'another after dispose'}),
    );
  });

  it('disposes only one handler at a time', () => {
    const bus = createMessageBus();
    globalMockWs.simulateServerConnected();

    const onMessage1 = jest.fn();
    bus.onMessage(onMessage1);

    const onMessage2 = jest.fn();
    const disposable2 = bus.onMessage(onMessage2);

    const onMessage3 = jest.fn();
    bus.onMessage(onMessage3);

    globalMockWs.simulateServerConnected();
    globalMockWs.simulateIncomingMessage('incoming message');

    expect(onMessage1).toHaveBeenCalledWith(expect.objectContaining({data: 'incoming message'}));
    expect(onMessage2).toHaveBeenCalledWith(expect.objectContaining({data: 'incoming message'}));
    expect(onMessage3).toHaveBeenCalledWith(expect.objectContaining({data: 'incoming message'}));

    disposable2.dispose();

    globalMockWs.simulateIncomingMessage('another after dispose');
    expect(onMessage2).not.toHaveBeenCalledWith(
      expect.objectContaining({data: 'another after dispose'}),
    );
    // the other handlers still active
    expect(onMessage1).toHaveBeenCalledWith(
      expect.objectContaining({data: 'another after dispose'}),
    );
    expect(onMessage3).toHaveBeenCalledWith(
      expect.objectContaining({data: 'another after dispose'}),
    );
  });

  it('can send messages as soon as connection is created', () => {
    const bus = createMessageBus();
    bus.onChangeStatus(newStatus => {
      if (newStatus.type === 'open') {
        bus.postMessage('message once connected');
      }
    });
    globalMockWs.simulateServerConnected();

    expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, 'message once connected']);
  });

  it('includes token from initialState', () => {
    createMessageBus();
    expect(globalMockWs.url).toEqual(`ws://${DEFAULT_HOST}/ws?token=1234&platform=test`);
  });

  describe('reconnect timing', () => {
    beforeEach(() => {
      jest.useFakeTimers();
    });
    afterEach(() => {
      jest.useRealTimers();
    });

    it('waits for an application response before opening and flushing queued messages', () => {
      const bus = createMessageBus();
      const status = jest.fn();
      bus.onChangeStatus(status);
      bus.postMessage('queued operation');
      globalMockWs.simulateTransportConnected();
      expect(globalMockWs.sentMessages).toEqual([HEARTBEAT]);
      expect(status).toHaveBeenLastCalledWith({type: 'initializing'});
      globalMockWs.simulateIncomingMessage('not an ISL message');
      expect(status).toHaveBeenCalledTimes(1);
      globalMockWs.simulateIncomingMessage(HEARTBEAT);
      expect(status).toHaveBeenLastCalledWith({type: 'open'});
      expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, 'queued operation']);
      bus.dispose();
    });

    it('retries promptly when sending the readiness heartbeat fails', () => {
      const bus = createMessageBus();
      const status = jest.fn();
      bus.onChangeStatus(status);
      bus.postMessage('queued operation');
      const failed = globalMockWs;
      jest.spyOn(failed, 'send').mockImplementation(() => {
        throw new Error('injected heartbeat send failure');
      });
      const close = jest.spyOn(failed, 'close');
      failed.simulateTransportConnected();

      expect(close).toHaveBeenCalledTimes(1);
      expect(status).toHaveBeenLastCalledWith({type: 'reconnecting'});
      expect(status).not.toHaveBeenCalledWith({type: 'open'});
      jest.advanceTimersByTime(99);
      expect(globalMockWs).toBe(failed);
      jest.advanceTimersByTime(1);
      expect(globalMockWs).not.toBe(failed);
      globalMockWs.simulateServerConnected();
      expect(globalMockWs.sentMessages).toEqual([HEARTBEAT, 'queued operation']);
      expect(status).toHaveBeenLastCalledWith({type: 'open'});
      bus.dispose();
      expect(jest.getTimerCount()).toBe(0);
    });

    it('keeps the pending retry when forceDisconnect is called on an already-closed socket', () => {
      const bus = createMessageBus();
      globalMockWs.simulateServerConnected();
      globalMockWs.simulateServerDisconnected();
      const closed = globalMockWs;
      closed.readyState = closed.CLOSED;
      // Native close() does not emit another event once the socket is closed.
      jest.spyOn(closed, 'close').mockImplementation(() => {});
      bus.forceDisconnect();

      jest.advanceTimersByTime(100);
      expect(globalMockWs).not.toBe(closed);
      globalMockWs.simulateServerConnected();
      bus.dispose();
      expect(jest.getTimerCount()).toBe(0);
    });

    it.each([0, 1])('retries unsent queued messages after send %i fails', failureIndex => {
      const bus = createMessageBus();
      const status = jest.fn();
      bus.onChangeStatus(status);
      bus.onChangeStatus(next => {
        if (next.type === 'open') {
          bus.postMessage('setup');
        }
      });
      globalMockWs.simulateServerConnected();
      globalMockWs.simulateServerDisconnected();
      status.mockClear();
      const queued = ['first', 'second', 'third'];
      queued.forEach(message => bus.postMessage(message));
      jest.advanceTimersByTime(100);

      const failed = globalMockWs;
      failed.simulateTransportConnected();
      const send = failed.send.bind(failed);
      jest.spyOn(failed, 'send').mockImplementation(message => {
        if (message === queued[failureIndex]) {
          throw new Error('injected send failure');
        }
        send(message);
      });
      const close = jest.spyOn(failed, 'close');
      failed.simulateIncomingMessage(HEARTBEAT);

      expect(close).toHaveBeenCalledTimes(1);
      expect(failed.sentMessages).toEqual([HEARTBEAT, ...queued.slice(0, failureIndex)]);
      expect(status).not.toHaveBeenCalled();
      jest.advanceTimersByTime(199);
      expect(globalMockWs).toBe(failed);
      jest.advanceTimersByTime(1);
      expect(globalMockWs).not.toBe(failed);
      globalMockWs.simulateServerConnected();
      expect(globalMockWs.sentMessages).toEqual([
        HEARTBEAT,
        ...queued.slice(failureIndex),
        'setup',
      ]);
      expect(status.mock.calls).toEqual([[{type: 'open'}]]);
      bus.dispose();
      expect(jest.getTimerCount()).toBe(0);
    });

    it('delivers the first application message after notifying setup listeners', () => {
      const bus = createMessageBus();
      const events: string[] = [];
      bus.onChangeStatus(status => {
        events.push(status.type);
      });
      bus.onMessage(() => {
        events.push('message');
      });
      globalMockWs.simulateTransportConnected();
      globalMockWs.simulateIncomingMessage(serializeToString({type: 'repoInfo'}));
      expect(events).toEqual(['initializing', 'open', 'message']);
      bus.dispose();
    });

    it('keeps reconnecting and backs off when a proxy opens then closes without a response', () => {
      const bus = createMessageBus();
      globalMockWs.simulateServerConnected();
      const status = jest.fn();
      bus.onChangeStatus(status);
      globalMockWs.simulateServerDisconnected();
      jest.advanceTimersByTime(100);
      globalMockWs.simulateTransportConnected();
      globalMockWs.simulateServerDisconnected(1011, 'upstream unavailable');
      const failed = globalMockWs;
      jest.advanceTimersByTime(100);
      expect(globalMockWs).toBe(failed);
      expect(status.mock.calls).toEqual([[{type: 'open'}], [{type: 'reconnecting'}]]);
      jest.advanceTimersByTime(100);
      expect(globalMockWs).not.toBe(failed);
      globalMockWs.simulateServerConnected();
      expect(status).toHaveBeenLastCalledWith({type: 'open'});
      bus.dispose();
    });

    it('retries a socket that opens but never answers the probe', () => {
      const bus = createMessageBus();
      globalMockWs.simulateTransportConnected();
      const silent = globalMockWs;
      jest.advanceTimersByTime(LocalWebSocketEventBus.CONNECTION_TIMEOUT_MS);
      jest.advanceTimersByTime(100);
      expect(globalMockWs).not.toBe(silent);
      bus.dispose();
    });

    it('stops retrying permanent errors and ignores events after disposal', () => {
      const bus = createMessageBus();
      const status = jest.fn();
      bus.onChangeStatus(status);
      globalMockWs.simulateTransportConnected();
      globalMockWs.simulateServerDisconnected(4100, 'Invalid token');
      const failed = globalMockWs;
      expect(status).toHaveBeenLastCalledWith({type: 'error', error: 'Invalid token'});
      jest.advanceTimersByTime(2 * LocalWebSocketEventBus.CONNECTION_TIMEOUT_MS);
      expect(globalMockWs).toBe(failed);
      bus.dispose();
      globalMockWs.simulateServerConnected();
      expect(status).toHaveBeenLastCalledWith({type: 'error', error: 'Invalid token'});
    });

    it('reconnects after a delay', () => {
      createMessageBus();

      const initialWs = globalMockWs;

      globalMockWs.simulateServerConnected();
      globalMockWs.simulateServerDisconnected();
      expect(initialWs).toBe(globalMockWs);
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS);
      // we have a new WebSocket instance which will re-try to connect
      expect(initialWs).not.toBe(globalMockWs);
    });

    it("doesn't reconnect after disposing", () => {
      const bus = createMessageBus();

      const previousWs = globalMockWs;
      globalMockWs.simulateServerConnected();
      bus.dispose();
      globalMockWs.simulateServerDisconnected();
      expect(previousWs).toBe(globalMockWs);
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS);
      expect(previousWs).toBe(globalMockWs); // we haven't made a new WebSocket, because we didn't try to reconnect
    });

    it('reconnects with exponential backoff', () => {
      createMessageBus();

      const initialWs = globalMockWs;

      globalMockWs.simulateServerConnected();
      globalMockWs.simulateServerDisconnected();
      expect(initialWs).toBe(globalMockWs);
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS + 10);
      expect(initialWs).not.toBe(globalMockWs);

      const nextWs = globalMockWs;

      // we failed to connect again
      globalMockWs.simulateServerDisconnected();
      // with exponential backoff, we should need to wait another tick
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS + 10);
      expect(nextWs).toBe(globalMockWs);
      // but after another round, we're past the doubled time
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS + 10);
      expect(nextWs).not.toBe(globalMockWs);
    });

    it('resets exponential backoff after a successful connection', () => {
      createMessageBus();

      globalMockWs.simulateServerConnected();

      // simulate 2 disconnects, which doubles backoff time
      globalMockWs.simulateServerDisconnected();
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS + 10);
      globalMockWs.simulateServerDisconnected();
      jest.advanceTimersByTime(2 * LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS + 10);

      // now reconnect should reset backoff time
      globalMockWs.simulateServerConnected();

      const initialWs = globalMockWs;
      globalMockWs.simulateServerDisconnected();
      expect(initialWs).toBe(globalMockWs);
      // advancing by initial reconnect time creates a new ws
      jest.advanceTimersByTime(LocalWebSocketEventBus.DEFAULT_RECONNECT_CHECK_TIME_MS + 10);
      expect(initialWs).not.toBe(globalMockWs);
    });

    it('caps out exponential backoff at maximum', () => {
      createMessageBus();

      globalMockWs.simulateServerConnected();

      // simulate a bunch of unsuccessful reconnects over time
      for (let i = 0; i < 100; i++) {
        globalMockWs.simulateServerDisconnected();
        jest.advanceTimersByTime(LocalWebSocketEventBus.MAX_RECONNECT_CHECK_TIME_MS);
      }

      // now backoff time should have stopped doubling

      const initialWs = globalMockWs;
      globalMockWs.simulateServerDisconnected();
      expect(initialWs).toBe(globalMockWs);
      // advancing by anything less than cap of reconnect time doesn't reconnect yet
      jest.advanceTimersByTime(LocalWebSocketEventBus.MAX_RECONNECT_CHECK_TIME_MS - 10);
      expect(initialWs).toBe(globalMockWs);

      // but just a little further pushes us over the edge and we reconnect
      jest.advanceTimersByTime(20);
      expect(initialWs).not.toBe(globalMockWs);
    });
  });
});
