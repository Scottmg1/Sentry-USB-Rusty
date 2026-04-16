import { useState, useEffect, useRef, useCallback } from "react"
import { Terminal as TerminalIcon, Loader2, LogIn, Power, AlertCircle } from "lucide-react"
import { Terminal as XTerm } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import "@xterm/xterm/css/xterm.css"

type ConnectionState = "login" | "connecting" | "connected" | "disconnected" | "error"

export default function TerminalPage() {
    const [state, setState] = useState<ConnectionState>("login")
    const [username, setUsername] = useState("")
    const [password, setPassword] = useState("")
    const [errorMsg, setErrorMsg] = useState("")

    const termRef = useRef<HTMLDivElement>(null)
    const xtermRef = useRef<XTerm | null>(null)
    const fitRef = useRef<FitAddon | null>(null)
    const wsRef = useRef<WebSocket | null>(null)
    const pendingOutputRef = useRef<string[]>([])

    const disconnect = useCallback(() => {
        if (wsRef.current) {
            wsRef.current.close()
            wsRef.current = null
        }
        if (xtermRef.current) {
            xtermRef.current.dispose()
            xtermRef.current = null
        }
        fitRef.current = null
        setUsername("")
        setPassword("")
        setState("disconnected")
    }, [])

    const connect = useCallback(() => {
        if (!username.trim() || !password) return

        setState("connecting")
        setErrorMsg("")
        pendingOutputRef.current = []

        const proto = window.location.protocol === "https:" ? "wss:" : "ws:"
        const ws = new WebSocket(`${proto}//${window.location.host}/api/terminal`)
        wsRef.current = ws

        ws.onopen = () => {
            ws.send(JSON.stringify({ type: "auth", username: username.trim(), password }))
        }

        ws.onmessage = (ev) => {
            let msg: { type: string; data?: string }
            try {
                msg = JSON.parse(ev.data)
            } catch {
                return
            }

            switch (msg.type) {
                case "auth_ok":
                    setState("connected")
                    break
                case "auth_failed":
                    setState("error")
                    setErrorMsg(msg.data || "Invalid username or password")
                    wsRef.current = null
                    ws.close()
                    break
                case "output":
                    if (msg.data) {
                        if (xtermRef.current) {
                            xtermRef.current.write(msg.data)
                        } else {
                            // Buffer output until xterm is initialized
                            pendingOutputRef.current.push(msg.data)
                        }
                    }
                    break
                case "exit":
                    disconnect()
                    break
                case "error":
                    setState("error")
                    setErrorMsg(msg.data || "Terminal error")
                    wsRef.current = null
                    ws.close()
                    break
            }
        }

        ws.onclose = (ev) => {
            if (wsRef.current) {
                if (ev.code === 1006) {
                    setState("error")
                    setErrorMsg("Connection lost. Check your network and try again.")
                    wsRef.current = null
                } else {
                    disconnect()
                }
            }
        }

        ws.onerror = () => {
            setState("error")
            setErrorMsg("WebSocket connection failed")
        }
    }, [username, password, disconnect])

    // Initialize xterm AFTER React renders the terminal container div
    useEffect(() => {
        if (state !== "connected" || !termRef.current || xtermRef.current) return
        const ws = wsRef.current
        if (!ws) return

        const term = new XTerm({
            cursorBlink: true,
            fontSize: 14,
            fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', Menlo, Monaco, 'Courier New', monospace",
            theme: {
                background: "#0b1120",
                foreground: "#cbd5e1",
                cursor: "#60a5fa",
                selectionBackground: "#334155",
                black: "#1e293b",
                red: "#ef4444",
                green: "#22c55e",
                yellow: "#eab308",
                blue: "#3b82f6",
                magenta: "#a855f7",
                cyan: "#06b6d4",
                white: "#e2e8f0",
                brightBlack: "#475569",
                brightRed: "#f87171",
                brightGreen: "#4ade80",
                brightYellow: "#facc15",
                brightBlue: "#60a5fa",
                brightMagenta: "#c084fc",
                brightCyan: "#22d3ee",
                brightWhite: "#f8fafc",
            },
            allowProposedApi: true,
        })

        const fit = new FitAddon()
        term.loadAddon(fit)
        term.open(termRef.current)
        fit.fit()

        xtermRef.current = term
        fitRef.current = fit

        // Flush any buffered output that arrived before xterm was ready
        for (const chunk of pendingOutputRef.current) {
            term.write(chunk)
        }
        pendingOutputRef.current = []

        // Send terminal size
        const dims = fit.proposeDimensions()
        if (dims && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: "resize", cols: dims.cols, rows: dims.rows }))
        }

        // Input → WebSocket
        term.onData((data) => {
            if (ws.readyState === WebSocket.OPEN) {
                ws.send(JSON.stringify({ type: "input", data }))
            }
        })

        // Handle resize
        const container = termRef.current
        const resizeObserver = new ResizeObserver(() => {
            if (fitRef.current && xtermRef.current) {
                fitRef.current.fit()
                const d = fitRef.current.proposeDimensions()
                if (d && ws.readyState === WebSocket.OPEN) {
                    ws.send(JSON.stringify({ type: "resize", cols: d.cols, rows: d.rows }))
                }
            }
        })
        resizeObserver.observe(container)

        term.focus()

        return () => {
            resizeObserver.disconnect()
        }
    }, [state])

    // Cleanup on unmount
    useEffect(() => {
        return () => {
            if (wsRef.current) {
                wsRef.current.close()
                wsRef.current = null
            }
            if (xtermRef.current) {
                xtermRef.current.dispose()
                xtermRef.current = null
            }
        }
    }, [])

    function handleSubmit(e: React.FormEvent) {
        e.preventDefault()
        connect()
    }

    // Login / disconnected view
    if (state !== "connected") {
        return (
            <div className="flex h-[calc(100vh-120px)] flex-col items-center justify-center md:h-[calc(100vh-96px)]">
                <div className="glass-card w-full max-w-sm p-6">
                    <div className="mb-6 flex flex-col items-center">
                        <div className="mb-3 flex h-14 w-14 items-center justify-center rounded-full bg-blue-500/15">
                            <TerminalIcon className="h-7 w-7 text-blue-400" />
                        </div>
                        <h1 className="text-lg font-semibold text-slate-100">Terminal</h1>
                        <p className="mt-1 text-center text-sm text-slate-500">
                            {state === "disconnected"
                                ? "Session ended. Log in again to reconnect."
                                : state === "error"
                                ? "Authentication failed. Check your credentials and try again."
                                : "Enter your Linux credentials to open a terminal session."
                            }
                        </p>
                    </div>

                    {(state === "error" && errorMsg) && (
                        <div className="mb-4 flex items-center gap-2 rounded-lg bg-red-500/10 px-3 py-2 text-sm text-red-400">
                            <AlertCircle className="h-4 w-4 shrink-0" />
                            {errorMsg}
                        </div>
                    )}

                    <form onSubmit={handleSubmit} className="space-y-3">
                        <div>
                            <label htmlFor="ssh-username" className="mb-1 block text-xs font-medium text-slate-400">Username</label>
                            <input
                                id="ssh-username"
                                name="username"
                                type="text"
                                value={username}
                                onChange={(e) => setUsername(e.target.value)}
                                placeholder="pi"
                                autoFocus
                                autoComplete="username"
                                className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2.5 text-sm text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
                            />
                        </div>
                        <div>
                            <label htmlFor="ssh-password" className="mb-1 block text-xs font-medium text-slate-400">Password</label>
                            <input
                                id="ssh-password"
                                name="password"
                                type="password"
                                value={password}
                                onChange={(e) => setPassword(e.target.value)}
                                placeholder="••••••••"
                                autoComplete="current-password"
                                className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2.5 text-sm text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
                            />
                        </div>
                        <button
                            type="submit"
                            disabled={state === "connecting" || !username.trim() || !password}
                            className="flex w-full items-center justify-center gap-2 rounded-lg bg-blue-500 px-4 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-600 disabled:opacity-50"
                        >
                            {state === "connecting" ? (
                                <>
                                    <Loader2 className="h-4 w-4 animate-spin" />
                                    Connecting...
                                </>
                            ) : (
                                <>
                                    <LogIn className="h-4 w-4" />
                                    Connect
                                </>
                            )}
                        </button>
                    </form>

                    <p className="mt-4 text-center text-[11px] text-slate-600">
                        Uses the same credentials as SSH
                    </p>
                </div>
            </div>
        )
    }

    // Connected terminal view
    return (
        <div className="flex h-[calc(100vh-120px)] flex-col md:h-[calc(100vh-96px)]">
            {/* Header */}
            <div className="mb-2 flex items-center justify-between">
                <div className="flex items-center gap-2">
                    <div className="h-2 w-2 rounded-full bg-emerald-400 shadow-[0_0_6px_rgba(52,211,153,0.5)]" />
                    <span className="text-sm font-medium text-slate-300">
                        {username}@sentryusb
                    </span>
                </div>
                <button
                    onClick={disconnect}
                    className="flex items-center gap-1.5 rounded-lg border border-white/10 px-3 py-1.5 text-xs font-medium text-slate-400 transition-colors hover:bg-red-500/10 hover:text-red-400"
                >
                    <Power className="h-3.5 w-3.5" />
                    Disconnect
                </button>
            </div>

            {/* Terminal container */}
            <div
                ref={termRef}
                className="flex-1 overflow-hidden rounded-lg border border-white/5 bg-[#0b1120]"
                style={{ padding: "4px" }}
            />
        </div>
    )
}
