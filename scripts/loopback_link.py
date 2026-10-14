"""Bounded duplex bandwidth/latency relay for disposable namespace benchmarks."""
import asyncio
import socket
import threading
import time


class LinkRelay:
    def __init__(self, mbps, delay_ms, listen_port=33992, target_port=33991):
        self.rate = mbps * 1_000_000 / 8 if mbps else 0
        self.delay = delay_ms / 1000
        self.listen_port = listen_port
        self.target_port = target_port
        self.bytes = {'server_to_client': 0, 'client_to_server': 0}
        self.connections = set()
        self.ready = threading.Event()
        self.error = None

    async def direction(self, reader, writer, name):
        # StreamReader, this queue, transport and socket buffers each have
        # explicit bounds. No desktop data is retained after this connection.
        queue = asyncio.Queue(maxsize=2)

        async def receive():
            while True:
                data = await reader.read(16 * 1024)
                await queue.put((time.monotonic() + self.delay, data))
                if not data:
                    return

        async def send():
            next_send = time.monotonic()
            while True:
                ready, data = await queue.get()
                if not data:
                    return
                due = max(ready, next_send)
                await asyncio.sleep(max(0, due - time.monotonic()))
                writer.write(data)
                await writer.drain()
                self.bytes[name] += len(data)
                next_send = max(due, time.monotonic()) + (len(data) / self.rate if self.rate else 0)

        tasks = [asyncio.create_task(receive()), asyncio.create_task(send())]
        try:
            await asyncio.gather(*tasks)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def connection(self, reader, writer):
        task = asyncio.current_task()
        if len(self.connections) >= 2:
            writer.close()
            return
        self.connections.add(task)
        upstream = None
        tasks = []
        try:
            remote_reader, upstream = await asyncio.open_connection('127.0.0.1', self.target_port, limit=16 * 1024)
            for stream in (writer, upstream):
                stream.transport.set_write_buffer_limits(high=16 * 1024, low=4096)
                sock = stream.get_extra_info('socket')
                # Tiny fixed TCP receive windows suppress autotuning and can
                # turn an unshaped loopback into a delayed-ACK/window bottleneck.
                # Keep finite kernel buffers large enough for the shaped BDP.
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 256 * 1024)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 256 * 1024)
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            tasks = [asyncio.create_task(self.direction(reader, upstream, 'client_to_server')),
                     asyncio.create_task(self.direction(remote_reader, writer, 'server_to_client'))]
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        except (OSError, asyncio.CancelledError):
            pass
        finally:
            for child in tasks:
                child.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            writer.close()
            if upstream:
                upstream.close()
            self.connections.discard(task)

    async def run(self):
        self.loop = asyncio.get_running_loop()
        self.stop = asyncio.Event()
        async with await asyncio.start_server(self.connection, '127.0.0.1', self.listen_port, limit=16 * 1024) as server:
            self.listen_port = server.sockets[0].getsockname()[1]
            self.ready.set()
            await self.stop.wait()
            for task in list(self.connections):
                task.cancel()
            await asyncio.gather(*self.connections, return_exceptions=True)

    def worker(self):
        try:
            asyncio.run(self.run())
        except Exception as error:
            self.error = error
            self.ready.set()

    def __enter__(self):
        self.thread = threading.Thread(target=self.worker, name='private-link-relay')
        self.thread.start()
        if not self.ready.wait(3) or self.error:
            raise RuntimeError('private link relay startup failed') from self.error
        return self

    def __exit__(self, *_):
        if self.thread.is_alive():
            self.loop.call_soon_threadsafe(self.stop.set)
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            raise RuntimeError('private link relay teardown deadline')
