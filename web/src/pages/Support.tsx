import { useState, useEffect, useRef, useCallback } from "react"
import {
  MessageCircle,
  Send,
  FileText,
  Paperclip,
  X,
  Loader2,
  CheckCircle,
  AlertCircle,
  WifiOff,
  Plus,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { BACKEND_BASE_URL } from "@/lib/api"

const POLL_INTERVAL = 3500
const STORAGE_KEY = "sentryusb_support_ticket"

interface Ticket {
  ticketId: string
  authToken: string
  threadId?: string
}

interface ChatMessage {
  id?: string
  sender: "user" | "support"
  content?: string
  timestamp: string
  responder?: string
  attachments?: { name: string; url: string; type: string }[]
  hasDiagnostics?: boolean
  read?: boolean
}

function escapeHtml(text: string) {
  const div = document.createElement("div")
  div.textContent = text
  return div.innerHTML
}

function formatFileSize(bytes: number) {
  if (bytes === 0) return "0 B"
  const k = 1024
  const sizes = ["B", "KB", "MB", "GB"]
  const i = Math.floor(Math.log(bytes) / Math.log(k))
  return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + " " + sizes[i]
}

export default function Support() {
  const [available, setAvailable] = useState<boolean | null>(null)
  const [ticket, setTicket] = useState<Ticket | null>(() => {
    try {
      const stored = localStorage.getItem(STORAGE_KEY)
      return stored ? JSON.parse(stored) : null
    } catch { return null }
  })
  const [messages, setMessages] = useState<ChatMessage[]>([])
  const [message, setMessage] = useState("")
  const [includeDiagnostics, setIncludeDiagnostics] = useState(false)
  const [attachment, setAttachment] = useState<File | null>(null)
  const [sending, setSending] = useState(false)
  const [status, setStatus] = useState<{ text: string; type: "loading" | "success" | "error" } | null>(null)
  const [ticketClosed, setTicketClosed] = useState(false)
  const [lastMessageId, setLastMessageId] = useState<string | null>(null)

  const messagesEndRef = useRef<HTMLDivElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  // Check if support server is reachable
  useEffect(() => {
    fetch("/api/support/check")
      .then(r => r.json())
      .then(data => setAvailable(data.available))
      .catch(() => setAvailable(false))
  }, [])

  // Scroll to bottom on new messages
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth" })
  }, [messages])

  // Fetch messages
  const fetchMessages = useCallback(async () => {
    if (!ticket) return
    try {
      const query = lastMessageId ? `?since=${lastMessageId}` : ""
      const res = await fetch(`/api/support/ticket/${ticket.ticketId}/messages${query}`, {
        headers: { "X-Auth-Token": ticket.authToken },
      })
      const data = await res.json()
      if (data.success && data.messages) {
        if (data.messages.length > 0) {
          setMessages(prev => {
            const existingIds = new Set(prev.map(m => m.id))
            const newMsgs = data.messages.filter((m: ChatMessage) => !m.id || !existingIds.has(m.id))
            return [...prev, ...newMsgs]
          })
          const last = data.messages[data.messages.length - 1]
          if (last.id) setLastMessageId(last.id)

          // Mark as read
          fetch(`/api/support/ticket/${ticket.ticketId}/mark-read`, {
            method: "POST",
            headers: { "X-Auth-Token": ticket.authToken },
          }).catch(() => { })
        }
        if (data.status === "closed") {
          setTicketClosed(true)
          if (pollRef.current) { clearInterval(pollRef.current); pollRef.current = null }
        }
      }
    } catch { /* ignore */ }
  }, [ticket, lastMessageId])

  // Poll for messages
  useEffect(() => {
    if (!ticket || ticketClosed) return
    fetchMessages()
    pollRef.current = setInterval(fetchMessages, POLL_INTERVAL)
    return () => { if (pollRef.current) clearInterval(pollRef.current) }
  }, [ticket, ticketClosed, fetchMessages])

  function saveTicket(t: Ticket | null) {
    setTicket(t)
    try {
      if (t) localStorage.setItem(STORAGE_KEY, JSON.stringify(t))
      else localStorage.removeItem(STORAGE_KEY)
    } catch { }
  }

  async function handleSend() {
    if (!message.trim() && !attachment) return
    setSending(true)
    setStatus({ text: "Sending...", type: "loading" })

    try {
      let diagnostics: string | null = null
      if (includeDiagnostics) {
        setStatus({ text: "Collecting diagnostics...", type: "loading" })
        // Refresh diagnostics first
        await fetch("/api/diagnostics/refresh", { method: "POST" }).catch(() => { })
        const diagRes = await fetch("/api/diagnostics")
        let rawDiag = await diagRes.text()
        // Sanitize: strip ANSI escape codes and replace lone surrogates / invalid chars
        // that would break PostgreSQL JSONB on the support server
        rawDiag = rawDiag.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, "") // ANSI escapes
        rawDiag = rawDiag.replace(/[\uD800-\uDFFF]/g, "\uFFFD") // lone surrogates
        rawDiag = rawDiag.replace(/[\x00-\x08\x0B\x0C\x0E-\x1F]/g, "") // control chars (keep \t \n \r)
        diagnostics = rawDiag
      }

      // Track the active ticket for this send (needed because setState is async)
      let activeTicket = ticket

      if (!activeTicket) {
        // Create new ticket
        setStatus({ text: "Creating support ticket...", type: "loading" })
        const res = await fetch("/api/support/ticket", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            message: message.trim(),
            diagnostics,
            hasAttachments: !!attachment,
          }),
        })
        const data = await res.json()
        if (!data.success && !data.ticketId) throw new Error(data.error || "Failed to create ticket")

        activeTicket = {
          ticketId: data.ticketId,
          authToken: data.authToken,
          threadId: data.threadId,
        }
        saveTicket(activeTicket)
      } else {
        // Send message to existing ticket
        if (!attachment) {
          setStatus({ text: "Sending message...", type: "loading" })
          const res = await fetch(`/api/support/ticket/${activeTicket.ticketId}/message`, {
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              "X-Auth-Token": activeTicket.authToken,
            },
            body: JSON.stringify({ message: message.trim(), diagnostics }),
          })
          const data = await res.json()
          if (!data.success) throw new Error(data.error || "Failed to send message")
        }
      }

      // Upload attachment if present
      if (attachment && activeTicket) {
        setStatus({ text: "Uploading attachment...", type: "loading" })
        const reader = new FileReader()
        const mediaData = await new Promise<string>((resolve, reject) => {
          reader.onload = () => resolve(reader.result as string)
          reader.onerror = reject
          reader.readAsDataURL(attachment)
        })

        await fetch(`/api/support/ticket/${activeTicket.ticketId}/media`, {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-Auth-Token": activeTicket.authToken,
          },
          body: JSON.stringify({
            mediaData,
            fileName: attachment.name,
            fileType: attachment.type,
            fileSize: attachment.size,
            message: message.trim(),
            diagnostics,
          }),
        })
      }

      // Clear form
      setMessage("")
      setIncludeDiagnostics(false)
      setAttachment(null)
      setStatus({ text: "Sent!", type: "success" })
      setTimeout(() => setStatus(null), 2000)

      // Refresh messages
      setTimeout(() => fetchMessages(), 500)
    } catch (err) {
      setStatus({ text: err instanceof Error ? err.message : "Failed to send", type: "error" })
    } finally {
      setSending(false)
    }
  }

  async function handleCloseTicket() {
    if (!ticket || !confirm("Close this support ticket? You can start a new one later.")) return
    try {
      await fetch(`/api/support/ticket/${ticket.ticketId}/close`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Auth-Token": ticket.authToken,
        },
        body: JSON.stringify({ closedBy: "User", reason: "Closed by user" }),
      })
    } catch { /* clear anyway */ }
    if (pollRef.current) { clearInterval(pollRef.current); pollRef.current = null }
    saveTicket(null)
    setMessages([])
    setLastMessageId(null)
    setTicketClosed(false)
  }

  function handleNewTicket() {
    saveTicket(null)
    setMessages([])
    setLastMessageId(null)
    setTicketClosed(false)
    setStatus(null)
  }

  // Offline state
  if (available === false) {
    return (
      <div className="flex h-[calc(100vh-120px)] flex-col items-center justify-center gap-4 md:h-[calc(100vh-96px)]">
        <WifiOff className="h-12 w-12 text-slate-600" />
        <h2 className="text-lg font-semibold text-slate-300">Support Unavailable</h2>
        <p className="max-w-md text-center text-sm text-slate-500">
          Cannot reach the support server. Make sure your device is connected to the internet, then refresh.
        </p>
        <button
          onClick={() => {
            setAvailable(null)
            fetch("/api/support/check")
              .then(r => r.json())
              .then(data => setAvailable(data.available))
              .catch(() => setAvailable(false))
          }}
          className="rounded-lg bg-blue-500/20 px-4 py-2 text-sm font-medium text-blue-400 transition-colors hover:bg-blue-500/30"
        >
          Retry
        </button>
      </div>
    )
  }

  if (available === null) {
    return (
      <div className="flex h-[calc(100vh-120px)] items-center justify-center md:h-[calc(100vh-96px)]">
        <Loader2 className="h-6 w-6 animate-spin text-slate-500" />
      </div>
    )
  }

  return (
    <div className="flex h-[calc(100vh-120px)] flex-col md:h-[calc(100vh-96px)]">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <MessageCircle className="h-5 w-5 text-blue-400" />
          <h1 className="text-lg font-semibold text-slate-100">Support Chat</h1>
          {ticket && (
            <span className="rounded bg-blue-500/15 px-2 py-0.5 text-xs font-medium text-blue-400">
              #{ticket.ticketId}
            </span>
          )}
        </div>
        {ticket && !ticketClosed && (
          <button
            onClick={handleCloseTicket}
            className="flex items-center gap-1.5 rounded-lg border border-white/10 px-3 py-1.5 text-xs font-medium text-slate-400 transition-colors hover:bg-white/5 hover:text-slate-200"
          >
            <CheckCircle className="h-3.5 w-3.5" />
            Close Ticket
          </button>
        )}
      </div>

      {/* Messages area */}
      <div className="mt-4 flex flex-1 flex-col overflow-hidden rounded-xl border border-white/5 bg-white/[0.02]">
        <div className="flex-1 overflow-y-auto px-4 py-4">
          {!ticket && messages.length === 0 && !ticketClosed && (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <div className="mb-4 flex h-16 w-16 items-center justify-center rounded-full bg-blue-500/15">
                <MessageCircle className="h-8 w-8 text-blue-400" />
              </div>
              <h3 className="text-lg font-semibold text-slate-200">Need Help?</h3>
              <p className="mt-2 max-w-md text-sm text-slate-500">
                Send a message below to start a support conversation. You can include
                device diagnostics and file attachments.
              </p>
              <p className="mt-3 text-xs text-slate-600">
                Your message creates a private support ticket. Responses will appear here.
              </p>
            </div>
          )}

          {messages.map((msg, i) => (
            <div
              key={msg.id || i}
              className={cn("mb-3 flex", msg.sender === "user" ? "justify-end" : "justify-start")}
            >
              {msg.sender !== "user" && (
                <div className="mr-2 flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-blue-500/20 text-xs font-bold text-blue-400">
                  S6
                </div>
              )}
              <div
                className={cn(
                  "max-w-[90%] rounded-xl px-4 py-2.5 sm:max-w-[75%]",
                  msg.sender === "user"
                    ? "bg-blue-500/20 text-slate-200"
                    : "bg-white/5 text-slate-300"
                )}
              >
                {msg.sender !== "user" && msg.responder && (
                  <p className="mb-1 text-[10px] font-semibold text-blue-400">{msg.responder}</p>
                )}
                {msg.content && (
                  <p className="whitespace-pre-wrap text-sm" dangerouslySetInnerHTML={{ __html: escapeHtml(msg.content) }} />
                )}
                {msg.hasDiagnostics && (
                  <span className="mt-1 inline-block rounded bg-white/5 px-2 py-0.5 text-[10px] text-slate-500">
                    📊 Diagnostics attached
                  </span>
                )}
                {msg.attachments && msg.attachments.length > 0 && (
                  <div className="mt-2 space-y-2">
                    {msg.attachments.map((a, j) => {
                      const url = a.url?.startsWith("http") ? a.url : `${BACKEND_BASE_URL}${a.url}`
                      const isImage = a.type?.startsWith("image/") || /\.(png|jpe?g|gif|webp|svg)$/i.test(a.name)
                      return isImage ? (
                        <a key={j} href={url} target="_blank" rel="noopener noreferrer" className="block">
                          <img
                            src={url}
                            alt={a.name}
                            className="max-h-64 rounded-lg border border-white/10 object-contain"
                            loading="lazy"
                          />
                        </a>
                      ) : (
                        <a
                          key={j}
                          href={url}
                          target="_blank"
                          rel="noopener noreferrer"
                          className="block rounded bg-white/5 px-2 py-1 text-xs text-blue-400 hover:bg-white/10"
                        >
                          📎 {a.name}
                        </a>
                      )
                    })}
                  </div>
                )}
                <p className="mt-1 text-[10px] text-slate-600">
                  {new Date(msg.timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
                </p>
              </div>
              {msg.sender === "user" && (
                <div className="ml-2 flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-slate-700/50 text-xs font-bold text-slate-400">
                  You
                </div>
              )}
            </div>
          ))}

          {ticketClosed && (
            <div className="flex flex-col items-center gap-3 py-8 text-center">
              <CheckCircle className="h-8 w-8 text-emerald-400" />
              <p className="text-sm text-slate-400">This ticket has been closed.</p>
              <button
                onClick={handleNewTicket}
                className="flex items-center gap-1.5 rounded-lg bg-blue-500/20 px-4 py-2 text-sm font-medium text-blue-400 hover:bg-blue-500/30"
              >
                <Plus className="h-4 w-4" /> Start New Conversation
              </button>
            </div>
          )}

          <div ref={messagesEndRef} />
        </div>

        {/* Composer */}
        {!ticketClosed && (
          <div className="shrink-0 border-t border-white/5 px-4 py-3">
            {/* Attachment preview */}
            {attachment && (
              <div className="mb-2 flex items-center gap-2 rounded-lg bg-white/5 px-3 py-1.5">
                <Paperclip className="h-3.5 w-3.5 text-slate-500" />
                <span className="flex-1 truncate text-xs text-slate-300">{attachment.name}</span>
                <span className="text-xs text-slate-600">{formatFileSize(attachment.size)}</span>
                <button onClick={() => setAttachment(null)} className="text-slate-500 hover:text-red-400">
                  <X className="h-3.5 w-3.5" />
                </button>
              </div>
            )}

            {/* Options row */}
            <div className="mb-2 flex items-center gap-3">
              <button
                type="button"
                onClick={() => setIncludeDiagnostics(!includeDiagnostics)}
                className={cn(
                  "flex items-center gap-1.5 rounded-lg px-2.5 py-1 text-xs font-medium transition-colors",
                  includeDiagnostics
                    ? "bg-blue-500/20 text-blue-400 ring-1 ring-blue-500/30"
                    : "text-slate-500 hover:bg-white/5 hover:text-slate-300"
                )}
              >
                <FileText className="h-3.5 w-3.5" />
                Diagnostics
              </button>
              <label className="flex cursor-pointer items-center gap-1.5 text-xs text-slate-500 hover:text-slate-300">
                <Paperclip className="h-3.5 w-3.5" />
                Attach
                <input
                  ref={fileInputRef}
                  type="file"
                  accept="image/*,video/*,.zip"
                  className="hidden"
                  onChange={e => {
                    const f = e.target.files?.[0]
                    if (f) {
                      if (f.size > 100 * 1024 * 1024) {
                        setStatus({ text: "File too large (max 100MB)", type: "error" })
                        return
                      }
                      setAttachment(f)
                    }
                    if (fileInputRef.current) fileInputRef.current.value = ""
                  }}
                />
              </label>
            </div>

            {/* Input row */}
            <div className="flex items-end gap-2">
              <textarea
                value={message}
                onChange={e => setMessage(e.target.value)}
                onKeyDown={e => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); handleSend() } }}
                placeholder="Type your message..."
                rows={2}
                maxLength={5000}
                className="flex-1 resize-none rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
              />
              <button
                onClick={handleSend}
                disabled={sending || (!message.trim() && !attachment)}
                className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-blue-500 text-white transition-colors hover:bg-blue-600 disabled:opacity-50"
              >
                {sending ? <Loader2 className="h-4 w-4 animate-spin" /> : <Send className="h-4 w-4" />}
              </button>
            </div>

            {/* Status + char count */}
            <div className="mt-1.5 flex items-center justify-between">
              {status ? (
                <span className={cn("flex items-center gap-1 text-xs", {
                  "text-blue-400": status.type === "loading",
                  "text-emerald-400": status.type === "success",
                  "text-red-400": status.type === "error",
                })}>
                  {status.type === "loading" && <Loader2 className="h-3 w-3 animate-spin" />}
                  {status.type === "success" && <CheckCircle className="h-3 w-3" />}
                  {status.type === "error" && <AlertCircle className="h-3 w-3" />}
                  {status.text}
                </span>
              ) : (
                <span className="text-[10px] text-slate-700">Shift+Enter for newline</span>
              )}
              <span className="text-[10px] text-slate-700">{message.length}/5000</span>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}
