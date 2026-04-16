type MessageHandler = (data: unknown) => void
type StatusListener = (connected: boolean) => void

class WebSocketClient {
  private ws: WebSocket | null = null
  private handlers: Map<string, Set<MessageHandler>> = new Map()
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private pingTimer: ReturnType<typeof setInterval> | null = null
  private url: string
  private _connected = false
  private statusListeners: Set<StatusListener> = new Set()

  constructor() {
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:"
    this.url = `${protocol}//${window.location.host}/api/ws`
  }

  get isConnected() {
    return this._connected
  }

  private setConnected(value: boolean) {
    if (this._connected === value) return
    this._connected = value
    this.statusListeners.forEach((cb) => cb(value))
  }

  onStatusChange(cb: StatusListener): () => void {
    this.statusListeners.add(cb)
    return () => { this.statusListeners.delete(cb) }
  }

  connect() {
    if (this.ws?.readyState === WebSocket.OPEN) return

    this.ws = new WebSocket(this.url)

    this.ws.onopen = () => {
      this.setConnected(true)
      this.startPing()
    }

    this.ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data) as { type: string; data: unknown }
        const handlers = this.handlers.get(msg.type)
        if (handlers) {
          handlers.forEach((handler) => handler(msg.data))
        }
      } catch {
        // ignore malformed messages
      }
    }

    this.ws.onclose = () => {
      this.stopPing()
      this.setConnected(false)
      this.scheduleReconnect()
    }

    this.ws.onerror = () => {
      this.ws?.close()
    }
  }

  private startPing() {
    this.stopPing()
    this.pingTimer = setInterval(() => {
      if (this.ws?.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: "ping" }))
      }
    }, 25000)
  }

  private stopPing() {
    if (this.pingTimer) {
      clearInterval(this.pingTimer)
      this.pingTimer = null
    }
  }

  private scheduleReconnect() {
    if (this.reconnectTimer) return
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, 3000)
  }

  subscribe(type: string, handler: MessageHandler): () => void {
    if (!this.handlers.has(type)) {
      this.handlers.set(type, new Set())
    }
    this.handlers.get(type)!.add(handler)

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this.connect()
    }

    return () => {
      this.handlers.get(type)?.delete(handler)
    }
  }

  reconnect() {
    this.disconnect()
    this.connect()
  }

  disconnect() {
    this.stopPing()
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }
    this.ws?.close()
    this.ws = null
  }
}

export const wsClient = new WebSocketClient()
