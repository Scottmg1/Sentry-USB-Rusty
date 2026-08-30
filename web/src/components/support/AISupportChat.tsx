import { useCallback, useEffect, useRef, useState } from "react"
import { useNavigate } from "react-router-dom"
import {
  CachedIcon,
  CheckIcon,
  CloseIcon,
  ContactSupportIcon,
  EncryptedIcon,
  ErrorIcon,
  GppMaybeIcon,
  GroupIcon,
  OpenInNewIcon,
  ProgressActivityIcon,
  SendIcon,
  SmartToyIcon,
  TaskIcon,
} from "@/components/icons"
import { cn, errorMessage } from "@/lib/utils"
import { AssistantMarkdown } from "./assistantMarkdown"
import {
  isSupportConversationGone,
  supportConversationResetReason,
  SUPPORT_DISCLOSURE_VERSION,
} from "@/api/support"
import type {
  ConversationStatus,
  FileRequestReference,
  SupportConversation,
  SupportFileRequest,
  SupportMessage,
  SupportTransport,
  SupportUiAction,
} from "@/api/support"

const DISCORD_URL = "https://discord.gg/9QZEzVwdnt"
const DEFAULT_STORAGE_KEY = "sentryusb_ai_support_conversation"
const MESSAGE_LIMIT = 5000
const LOCAL_DIAGNOSTIC_HARD_LIMIT = 2 * 1024 * 1024
const STORED_CONVERSATION_MAX_AGE_MS = 90 * 24 * 60 * 60 * 1000

interface StoredConversation extends SupportConversation {
  storedAt: number
  disclosureVersion: string
}

interface StoredConversationState {
  conversation: SupportConversation
  disclosureCurrent: boolean
}

interface AISupportChatProps {
  transport: SupportTransport
  storageKey?: string | null
}

type FileDecisionPhase =
  | "idle"
  | "approving"
  | "denying"
  | "approved"
  | "denied"
  | "error"

interface FileDecisionViewState {
  phase: FileDecisionPhase
  detail?: string
  retryDecision?: "approved" | "denied"
}

type UiActionViewState = {
  phase: "idle" | "working" | "success" | "error"
  detail?: string
  requiresReboot?: boolean
}

const LOCAL_UI_ACTIONS: Record<string, {
  kind: "navigate" | "setting" | "operation"
  label: string
  path?: string
  endpoint?: string
  confirmation?: string
}> = {
  "open-dashboard": { kind: "navigate", label: "Open Dashboard", path: "/" },
  "open-viewer": { kind: "navigate", label: "Open Viewer", path: "/viewer" },
  "open-files": { kind: "navigate", label: "Open Files", path: "/files" },
  "open-snapshots": { kind: "navigate", label: "Open Snapshots", path: "/snapshots" },
  "open-drives": { kind: "navigate", label: "Open Drives", path: "/drives" },
  "open-charging": { kind: "navigate", label: "Open Charging", path: "/charging" },
  "open-safety-score": { kind: "navigate", label: "Open Safety Score", path: "/safety" },
  "open-fsd-analytics": { kind: "navigate", label: "Open FSD Analytics", path: "/fsd" },
  "open-community": { kind: "navigate", label: "Open Community", path: "/community" },
  "open-lock-chimes": { kind: "navigate", label: "Open Lock Chimes", path: "/community?view=chimes" },
  "open-notifications": { kind: "navigate", label: "Open Notifications", path: "/notifications" },
  "open-terminal": { kind: "navigate", label: "Open Terminal", path: "/terminal" },
  "open-archiveloop-log": {
    kind: "navigate", label: "Open Archiveloop log", path: "/logs?tab=archiveloop",
  },
  "open-bluetooth-log": {
    kind: "navigate", label: "Open Bluetooth log", path: "/logs?tab=bluetooth",
  },
  "open-setup-log": { kind: "navigate", label: "Open Setup log", path: "/logs?tab=setup" },
  "open-diagnostics-log": {
    kind: "navigate", label: "Open Diagnostics", path: "/logs?tab=diagnostics",
  },
  "open-device-settings": {
    kind: "navigate", label: "Open Device settings", path: "/settings?tab=Device",
  },
  "open-car-network-settings": {
    kind: "navigate", label: "Open Car & Network settings", path: "/settings?tab=Car+%26+Network",
  },
  "open-travel-mode": {
    kind: "navigate", label: "Open Travel Mode", path: "/settings?tab=Car+%26+Network&travel=1",
  },
  "open-notifications-settings": {
    kind: "navigate", label: "Open Notifications & Community", path: "/settings?tab=Notifications+%26+Community",
  },
  "open-system-settings": {
    kind: "navigate", label: "Open System settings", path: "/settings?tab=System",
  },
  "open-archive-settings": {
    kind: "navigate", label: "Open Archive settings", path: "/settings?tab=System&wizard=archive",
  },
  "open-welcome-setup": {
    kind: "navigate", label: "Open Setup Wizard welcome", path: "/settings?tab=System&wizard=welcome",
  },
  "open-privacy-setup": {
    kind: "navigate", label: "Open Privacy setup", path: "/settings?tab=System&wizard=privacy",
  },
  "open-network-setup": {
    kind: "navigate", label: "Open Network setup", path: "/settings?tab=System&wizard=network",
  },
  "open-storage-setup": {
    kind: "navigate", label: "Open Storage setup", path: "/settings?tab=System&wizard=storage",
  },
  "open-community-setup": {
    kind: "navigate", label: "Open Community setup", path: "/settings?tab=System&wizard=community",
  },
  "open-keep-awake-setup": {
    kind: "navigate", label: "Open Keep Awake setup", path: "/settings?tab=System&wizard=keepawake",
  },
  "open-notifications-setup": {
    kind: "navigate", label: "Open Notifications setup", path: "/settings?tab=System&wizard=notifications",
  },
  "open-security-setup": {
    kind: "navigate", label: "Open Security setup", path: "/settings?tab=System&wizard=security",
  },
  "open-advanced-setup": {
    kind: "navigate", label: "Open Advanced setup", path: "/settings?tab=System&wizard=advanced",
  },
  "open-review-setup": {
    kind: "navigate", label: "Open Setup review", path: "/settings?tab=System&wizard=review",
  },
  "run-archive-sync": {
    kind: "operation",
    label: "Run Archive Sync",
    endpoint: "/api/system/trigger-sync",
    confirmation: "Trigger Archive Sync now? This starts a local archive cycle using your current archive configuration.",
  },
  "set-archive-sentry-without-recent": {
    kind: "setting",
    label: "Use Sentry Clips, not Recent Clips",
    confirmation:
      "Enable Sentry Clips archiving and disable Recent Clips archiving? Saved Clips and Track Mode will stay unchanged. The configuration will be saved locally and requires a Pi restart before the archive service uses it.",
  },
  "enable-archive-saved": {
    kind: "setting", label: "Enable Saved Clips archiving",
    confirmation: "Enable Saved Clips archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "disable-archive-saved": {
    kind: "setting", label: "Disable Saved Clips archiving",
    confirmation: "Disable Saved Clips archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "enable-archive-sentry": {
    kind: "setting", label: "Enable Sentry Clips archiving",
    confirmation: "Enable Sentry Clips archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "disable-archive-sentry": {
    kind: "setting", label: "Disable Sentry Clips archiving",
    confirmation: "Disable Sentry Clips archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "enable-archive-recent": {
    kind: "setting", label: "Enable Recent Clips archiving",
    confirmation: "Enable Recent Clips archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "disable-archive-recent": {
    kind: "setting", label: "Disable Recent Clips archiving",
    confirmation: "Disable Recent Clips archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "enable-archive-track-mode": {
    kind: "setting", label: "Enable Track Mode archiving",
    confirmation: "Enable Track Mode archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
  "disable-archive-track-mode": {
    kind: "setting", label: "Disable Track Mode archiving",
    confirmation: "Disable Track Mode archiving? Other archive categories will stay unchanged. The saved configuration requires a Pi restart.",
  },
}

function localUiAction(action: SupportUiAction) {
  const local = LOCAL_UI_ACTIONS[action.id]
  return local && local.kind === action.kind ? local : null
}

function readStoredConversation(storageKey: string | null): StoredConversationState | null {
  if (!storageKey) return null
  try {
    const raw = localStorage.getItem(storageKey)
    if (!raw) return null
    const parsed = JSON.parse(raw) as Partial<StoredConversation>
    const age = Date.now() - (parsed.storedAt ?? Number.NaN)
    if (
      typeof parsed.id !== "string"
      || typeof parsed.authToken !== "string"
      || !Number.isFinite(age)
      || age < 0
      || age > STORED_CONVERSATION_MAX_AGE_MS
    ) {
      localStorage.removeItem(storageKey)
      return null
    }
    return {
      conversation: { id: parsed.id, authToken: parsed.authToken },
      disclosureCurrent: parsed.disclosureVersion === SUPPORT_DISCLOSURE_VERSION,
    }
  } catch {
    return null
  }
}

function persistConversation(storageKey: string | null, conversation: SupportConversation | null) {
  if (!storageKey) return
  try {
    if (conversation) {
      const stored: StoredConversation = {
        ...conversation,
        storedAt: Date.now(),
        disclosureVersion: SUPPORT_DISCLOSURE_VERSION,
      }
      localStorage.setItem(storageKey, JSON.stringify(stored))
    }
    else localStorage.removeItem(storageKey)
  } catch {
    // A conversation can continue for this tab even if storage is unavailable.
  }
}

function clientMessageId() {
  const cryptoApi = globalThis.crypto as Partial<Crypto> | undefined
  if (typeof cryptoApi?.randomUUID === "function") {
    return `client-${cryptoApi.randomUUID()}`
  }
  if (typeof cryptoApi?.getRandomValues === "function") {
    const bytes = cryptoApi.getRandomValues(new Uint8Array(16))
    return `client-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`
  }
  return `client-${Date.now()}-${Math.random().toString(36).slice(2).padEnd(16, "0")}`
}

function mergeMessages(current: SupportMessage[], incoming: SupportMessage[]) {
  if (incoming.length === 0) return current
  const next = [...current]

  for (const message of incoming) {
    const exactIndex = next.findIndex((item) => item.id === message.id)
    if (exactIndex >= 0) {
      next[exactIndex] = message
      continue
    }

    // Replace the optimistic user bubble when the server assigns its own id.
    if (message.role === "user") {
      const optimisticIndex = next.findIndex(
        (item) => item.id.startsWith("client-") && item.role === "user" && item.content === message.content,
      )
      if (optimisticIndex >= 0) {
        next[optimisticIndex] = message
        continue
      }
    }

    next.push(message)
  }

  return next.sort((a, b) => {
    const aTime = Date.parse(a.createdAt)
    const bTime = Date.parse(b.createdAt)
    if (!Number.isFinite(aTime) || !Number.isFinite(bTime)) return 0
    return aTime - bTime
  })
}

function displayTime(value: string) {
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return ""
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
}

function formatBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "Unknown size"
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  return `${Math.ceil(bytes / 1024)} KB`
}

function filePolicyIssue(request: SupportFileRequest): string | null {
  if (request.kind !== "diagnostics") return "This file type is not available to AI Support."
  if (request.retentionDays !== 7) return "This request does not match the required 7-day retention policy."
  if (!Number.isFinite(request.maxBytes) || request.maxBytes <= 0) return "This request has an invalid size limit."
  if (!/^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/.test(request.fileName)) {
    return "This request contains an unsafe file name."
  }
  return null
}

export function AISupportChat({
  transport,
  storageKey = DEFAULT_STORAGE_KEY,
}: AISupportChatProps) {
  const navigate = useNavigate()
  const normalizedStorageKey = storageKey ?? null
  const [initialStored] = useState(() => readStoredConversation(normalizedStorageKey))
  const [conversation, setConversation] = useState<SupportConversation | null>(() =>
    initialStored?.conversation ?? null,
  )
  const [messages, setMessages] = useState<SupportMessage[]>([])
  const [draft, setDraft] = useState("")
  const [disclosureAccepted, setDisclosureAccepted] = useState(() =>
    initialStored?.disclosureCurrent === true,
  )
  const [requiresDisclosureReset, setRequiresDisclosureReset] = useState(() =>
    initialStored !== null && !initialStored.disclosureCurrent,
  )
  const [deletingStaleConversation, setDeletingStaleConversation] = useState(false)
  const [sending, setSending] = useState(false)
  const [typing, setTyping] = useState(false)
  const [conversationStatus, setConversationStatus] = useState<ConversationStatus>("idle")
  const [loadingConversation, setLoadingConversation] = useState(() =>
    conversation !== null && initialStored?.disclosureCurrent === true,
  )
  const [error, setError] = useState<string | null>(null)
  const [fileDecisions, setFileDecisions] = useState<Record<string, FileDecisionViewState>>({})
  const [uiActionStates, setUiActionStates] = useState<Record<string, UiActionViewState>>({})
  const messagesEndRef = useRef<HTMLDivElement>(null)
  const pendingSendRef = useRef<{ content: string; clientMessageId: string } | null>(null)

  const applyConversationResult = useCallback((nextMessages: SupportMessage[], status: ConversationStatus) => {
    setMessages((current) => mergeMessages(current, nextMessages))
    setConversationStatus(status)
    setTyping(status === "generating")
  }, [])

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth", block: "end" })
  }, [messages, typing])

  useEffect(() => {
    if (!conversation || requiresDisclosureReset) return
    let cancelled = false

    async function resumeConversation() {
      try {
        const result = await transport.fetchMessages(conversation as SupportConversation)
        if (cancelled) return
        applyConversationResult(result.messages, result.status)
        setError(null)
      } catch (err) {
        if (!cancelled) {
          const resetReason = supportConversationResetReason(err)
          if (resetReason === "disclosure") {
            setRequiresDisclosureReset(true)
            setDisclosureAccepted(false)
            setError(null)
          } else if (resetReason === "unavailable") {
            persistConversation(normalizedStorageKey, null)
            setConversation(null)
            setMessages([])
            setDisclosureAccepted(false)
            setError("This saved support conversation is no longer available. Start a new chat to continue.")
          } else {
            setError(errorMessage(err, "Could not resume this conversation."))
          }
        }
      } finally {
        if (!cancelled) setLoadingConversation(false)
      }
    }

    void resumeConversation()
    return () => {
      cancelled = true
    }
  }, [applyConversationResult, conversation, normalizedStorageKey, requiresDisclosureReset, transport])

  // Poll quickly only while the server says a response is being generated.
  // Idle conversations make no recurring requests.
  useEffect(() => {
    if (!conversation || conversationStatus !== "generating") return
    let cancelled = false
    let timer: ReturnType<typeof setTimeout> | undefined

    async function refreshWhileGenerating() {
      let continuePolling = true
      try {
        const result = await transport.fetchMessages(conversation as SupportConversation)
        continuePolling = result.status === "generating"
        if (!cancelled) applyConversationResult(result.messages, result.status)
      } catch (err) {
        const resetReason = supportConversationResetReason(err)
        if (!cancelled && resetReason === "disclosure") {
          continuePolling = false
          setRequiresDisclosureReset(true)
          setDisclosureAccepted(false)
          setTyping(false)
          setConversationStatus("idle")
          setError(null)
        } else if (!cancelled && resetReason === "unavailable") {
          continuePolling = false
          persistConversation(normalizedStorageKey, null)
          setConversation(null)
          setMessages([])
          setDisclosureAccepted(false)
          setTyping(false)
          setConversationStatus("idle")
          setError("This support conversation is no longer available. Start a new chat to continue.")
        }
        // Other transient failures retry only after this request has finished.
      }
      if (!cancelled && continuePolling) timer = setTimeout(() => { void refreshWhileGenerating() }, 2000)
    }

    timer = setTimeout(() => { void refreshWhileGenerating() }, 2000)
    return () => {
      cancelled = true
      if (timer) clearTimeout(timer)
    }
  }, [applyConversationResult, conversation, conversationStatus, normalizedStorageKey, transport])

  const sendMessage = useCallback(async () => {
    const content = draft.trim()
    if (
      !content
      || sending
      || requiresDisclosureReset
      || conversationStatus === "generating"
      || conversationStatus === "closed"
    ) return
    if (!conversation && !disclosureAccepted) {
      setError("Please confirm that you understand this is online AI support before sending.")
      return
    }

    const pending = pendingSendRef.current?.content === content
      ? pendingSendRef.current
      : { content, clientMessageId: clientMessageId() }
    pendingSendRef.current = pending
    const optimisticId = pending.clientMessageId
    const optimistic: SupportMessage = {
      id: optimisticId,
      role: "user",
      content,
      createdAt: new Date().toISOString(),
    }

    setMessages((current) => [...current, optimistic])
    setDraft("")
    setSending(true)
    setTyping(true)
    setConversationStatus("generating")
    setError(null)

    try {
      if (!conversation) {
        const result = await transport.createConversation(content, optimisticId)
        setConversation(result.conversation)
        persistConversation(normalizedStorageKey, result.conversation)
        applyConversationResult(result.messages, result.status)
      } else {
        const result = await transport.sendMessage(conversation, content, optimisticId)
        persistConversation(normalizedStorageKey, conversation)
        applyConversationResult(result.messages, result.status)
      }
      pendingSendRef.current = null
    } catch (err) {
      setMessages((current) => current.filter((message) => message.id !== optimisticId))
      setDraft(content)
      setTyping(false)
      setConversationStatus("idle")
      const resetReason = supportConversationResetReason(err)
      if (resetReason === "disclosure" && conversation) {
        setRequiresDisclosureReset(true)
        setDisclosureAccepted(false)
        setError(null)
      } else if (resetReason === "unavailable") {
        persistConversation(normalizedStorageKey, null)
        setConversation(null)
        setMessages([])
        setDisclosureAccepted(false)
        setError("This support conversation is no longer available. Acknowledge the current disclosure to start a new chat.")
      } else {
        setError(errorMessage(err, "The message could not be sent."))
      }
    } finally {
      setSending(false)
    }
  }, [
    applyConversationResult,
    disclosureAccepted,
    conversation,
    conversationStatus,
    draft,
    normalizedStorageKey,
    requiresDisclosureReset,
    sending,
    transport,
  ])

  const startNewConversation = useCallback(async () => {
    if (!conversation) return
    const confirmed = window.confirm(
      "End and permanently delete this conversation from Sentry Six servers? Any diagnostics approved for this conversation will also be deleted. This cannot be undone.",
    )
    if (!confirmed) return

    setError(null)
    try {
      await transport.deleteConversation(conversation)
      persistConversation(normalizedStorageKey, null)
      setConversation(null)
      setMessages([])
      setTyping(false)
      setConversationStatus("idle")
      setLoadingConversation(false)
      setFileDecisions({})
      setUiActionStates({})
      pendingSendRef.current = null
      setRequiresDisclosureReset(false)
      setDisclosureAccepted(false)
    } catch (err) {
      setError(errorMessage(err, "Could not end the conversation."))
    }
  }, [conversation, normalizedStorageKey, transport])

  const deleteStaleConversation = useCallback(async () => {
    if (!conversation || deletingStaleConversation) return
    setDeletingStaleConversation(true)
    setError(null)
    try {
      await transport.deleteConversation(conversation)
      persistConversation(normalizedStorageKey, null)
      setConversation(null)
      setMessages([])
      setTyping(false)
      setConversationStatus("idle")
      setLoadingConversation(false)
      setFileDecisions({})
      setUiActionStates({})
      pendingSendRef.current = null
      setRequiresDisclosureReset(false)
      setDisclosureAccepted(false)
    } catch (err) {
      if (isSupportConversationGone(err)) {
        persistConversation(normalizedStorageKey, null)
        setConversation(null)
        setMessages([])
        setTyping(false)
        setConversationStatus("idle")
        setLoadingConversation(false)
        setFileDecisions({})
        setUiActionStates({})
        pendingSendRef.current = null
        setRequiresDisclosureReset(false)
        setDisclosureAccepted(false)
        setError(null)
      } else {
        const detail = errorMessage(err, "The delete request failed.")
        setError(`${detail} Deletion was not verified. Retry, contact privacy@sentry-six.com, or explicitly forget local access; any server copy will otherwise expire under the retention policy.`)
      }
    } finally {
      setDeletingStaleConversation(false)
    }
  }, [conversation, deletingStaleConversation, normalizedStorageKey, transport])

  const forgetStaleConversation = useCallback(() => {
    persistConversation(normalizedStorageKey, null)
    setConversation(null)
    setMessages([])
    setTyping(false)
    setConversationStatus("idle")
    setLoadingConversation(false)
    setFileDecisions({})
    setUiActionStates({})
    pendingSendRef.current = null
    setRequiresDisclosureReset(false)
    setDisclosureAccepted(false)
    setError(null)
  }, [normalizedStorageKey])

  const handleFileDecision = useCallback(async (
    reference: FileRequestReference,
    decision: "approved" | "denied",
  ) => {
    if (!conversation) return
    const key = `${reference.messageId}:${reference.requestIndex}`
    const policyIssue = filePolicyIssue(reference.request)
    if (decision === "approved" && policyIssue) {
      setFileDecisions((current) => ({ ...current, [key]: { phase: "error", detail: policyIssue } }))
      return
    }

    setFileDecisions((current) => ({
      ...current,
      [key]: { phase: decision === "approved" ? "approving" : "denying" },
    }))

    try {
      if (decision === "denied") {
        await transport.decideFileRequest(conversation, reference, "denied")
        persistConversation(normalizedStorageKey, conversation)
        setFileDecisions((current) => ({ ...current, [key]: { phase: "denied" } }))
        return
      }

      const approved = await transport.decideFileRequest(conversation, reference, "approved")
      persistConversation(normalizedStorageKey, conversation)
      if (!approved.uploadToken) throw new Error("No one-time upload token was returned")
      const enforcedMax = Math.min(
        reference.request.maxBytes,
        approved.maxBytes ?? reference.request.maxBytes,
        LOCAL_DIAGNOSTIC_HARD_LIMIT,
      )
      // Consent is recorded first; only then does this explicit click allow
      // the local diagnostics endpoints to be touched.
      const diagnostics = await transport.collectDiagnostics(enforcedMax)
      if (diagnostics.size > enforcedMax) {
        throw new Error(`Diagnostics exceed the approved ${formatBytes(enforcedMax)} limit`)
      }

      const uploaded = await transport.uploadRequestedFile(
        conversation,
        reference,
        approved.uploadToken,
        diagnostics,
      )
      persistConversation(normalizedStorageKey, conversation)
      setTyping(true)
      setConversationStatus("generating")
      setFileDecisions((current) => ({
        ...current,
        [key]: {
          phase: "approved",
          detail: `${uploaded.fileName} (${formatBytes(uploaded.size)}) was shared once.`,
        },
      }))
    } catch (err) {
      const resetReason = supportConversationResetReason(err)
      if (resetReason === "disclosure") {
        setRequiresDisclosureReset(true)
        setDisclosureAccepted(false)
        setTyping(false)
        setConversationStatus("idle")
        setError(null)
      } else if (resetReason === "unavailable") {
        persistConversation(normalizedStorageKey, null)
        setConversation(null)
        setMessages([])
        setDisclosureAccepted(false)
        setTyping(false)
        setConversationStatus("idle")
        setError("This support conversation is no longer available. Acknowledge the current disclosure to start a new chat.")
      } else {
        setFileDecisions((current) => ({
          ...current,
          [key]: {
            phase: "error",
            detail: `${errorMessage(err, "The file was not shared.")} Nothing will retry automatically.`,
            retryDecision: decision,
          },
        }))
      }
    }
  }, [conversation, normalizedStorageKey, transport])

  const handleUiAction = useCallback(async (
    messageId: string,
    action: SupportUiAction,
  ) => {
    const local = localUiAction(action)
    const key = `${messageId}:${action.id}`
    if (!local) {
      setUiActionStates((current) => ({
        ...current,
        [key]: { phase: "error", detail: "This action is not available in this Sentry USB version." },
      }))
      return
    }
    if (local.kind === "navigate" && local.path) {
      navigate(local.path)
      return
    }
    if (!["setting", "operation"].includes(local.kind) || !local.confirmation || !window.confirm(local.confirmation)) return

    setUiActionStates((current) => ({ ...current, [key]: { phase: "working" } }))
    try {
      const endpoint = local.kind === "setting"
        ? `/api/support/local-actions/${encodeURIComponent(action.id)}`
        : local.endpoint
      if (!endpoint) throw new Error("This local action has no approved endpoint")
      const response = await fetch(endpoint, {
        method: "POST",
      })
      const data = await response.json().catch(() => null) as {
        summary?: string
        error?: string
        requiresReboot?: boolean
      } | null
      if (!response.ok) throw new Error(data?.error || `Action failed (${response.status})`)
      setUiActionStates((current) => ({
        ...current,
        [key]: {
          phase: "success",
          detail: data?.summary || (local.kind === "operation" ? "Archive Sync triggered." : "Setting applied."),
          requiresReboot: data?.requiresReboot === true,
        },
      }))
    } catch (error) {
      setUiActionStates((current) => ({
        ...current,
        [key]: {
          phase: "error",
          detail: errorMessage(error, local.kind === "operation" ? "The action did not run." : "The setting was not applied."),
        },
      }))
    }
  }, [navigate])

  const fileOperationInProgress = Object.values(fileDecisions).some(
    (state) => state.phase === "approving" || state.phase === "denying",
  )
  const composerLocked = sending
    || fileOperationInProgress
    || conversationStatus === "generating"
    || conversationStatus === "closed"
    || requiresDisclosureReset

  return (
    <div className="ai-chat flex h-[calc(100vh-120px)] min-h-[620px] flex-col md:h-[calc(100vh-96px)]">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-3">
          <div className="halo-accent flex h-9 w-9 shrink-0 items-center justify-center rounded-xl">
            <ContactSupportIcon className="h-5 w-5" />
          </div>
          <div className="min-w-0">
            <h1 className="text-lg font-semibold text-slate-100">AI Support &amp; Help</h1>
            <p className="text-xs text-slate-500">Product-specific help for Sentry USB Rusty</p>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <DiscordLink compact />
          {conversation && !requiresDisclosureReset && (
            <button type="button" onClick={() => { void startNewConversation() }} className="action-chip">
              <CachedIcon className="h-3.5 w-3.5" /> New chat
            </button>
          )}
        </div>
      </header>

      <section className="ai-chat-shell glass-card mt-3 flex min-h-0 flex-1 flex-col overflow-hidden" aria-label="AI support conversation">
        {conversation && requiresDisclosureReset ? (
          <DisclosureRefreshGate
            deleting={deletingStaleConversation}
            error={error}
            onDelete={() => { void deleteStaleConversation() }}
            onForget={forgetStaleConversation}
          />
        ) : !conversation && !disclosureAccepted ? (
          <DisclosureGate
            onAcknowledge={() => {
              setDisclosureAccepted(true)
              setError(null)
            }}
          />
        ) : (
          <>
            <div className="min-h-0 flex-1 overflow-y-auto px-3 py-4 sm:px-5" aria-live="polite">
              {loadingConversation && messages.length === 0 ? (
                <div className="flex h-full items-center justify-center gap-2 text-sm text-slate-500">
                  <ProgressActivityIcon className="h-4 w-4 animate-spin" /> Resuming conversation…
                </div>
              ) : messages.length === 0 ? (
                <EmptyConversation onSuggestion={setDraft} />
              ) : (
                messages.map((message) => (
                  <MessageBubble
                    key={message.id}
                    message={message}
                    fileDecisions={fileDecisions}
                    onFileDecision={handleFileDecision}
                    uiActionStates={uiActionStates}
                    onUiAction={handleUiAction}
                  />
                ))
              )}

              {typing && <TypingBubble />}
              <div ref={messagesEndRef} />
            </div>

            <div className="ai-chat-composer shrink-0 border-t border-white/5 bg-black/10 px-3 py-3 sm:px-4">
              {error && (
                <div className="mb-2 flex items-start gap-2 rounded-lg border border-rose-400/20 bg-rose-500/[0.07] px-3 py-2 text-xs text-rose-200" role="alert">
                  <ErrorIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  <span className="flex-1">{error}</span>
                  <button type="button" onClick={() => setError(null)} aria-label="Dismiss error" className="text-rose-300 hover:text-white">
                    <CloseIcon className="h-3.5 w-3.5" />
                  </button>
                </div>
              )}

              {conversationStatus === "closed" && (
                <p className="mb-2 text-xs text-slate-400" role="status">
                  This conversation is closed. Use New chat to delete it and start again.
                </p>
              )}

              <div className="flex items-end gap-2">
                <label className="sr-only" htmlFor="ai-support-message">Message AI Support</label>
                <textarea
                  id="ai-support-message"
                  value={draft}
                  disabled={composerLocked}
                  onChange={(event) => setDraft(event.target.value.slice(0, MESSAGE_LIMIT))}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" && !event.shiftKey) {
                      event.preventDefault()
                      void sendMessage()
                    }
                  }}
                  placeholder={conversationStatus === "generating" ? "Waiting for Sentry AI…" : "Ask about Sentry USB Rusty…"}
                  rows={2}
                  maxLength={MESSAGE_LIMIT}
                  className="min-h-11 flex-1 resize-none rounded-xl border border-white/10 bg-white/[0.04] px-3 py-2.5 text-sm text-slate-100 outline-none placeholder:text-slate-600 focus:border-blue-400/50 focus:ring-2 focus:ring-blue-400/10 disabled:cursor-not-allowed disabled:opacity-60"
                />
                <button
                  type="button"
                  onClick={() => { void sendMessage() }}
                  disabled={composerLocked || !draft.trim()}
                  aria-label="Send message"
                  className="flex h-11 w-11 shrink-0 items-center justify-center rounded-xl bg-blue-500 text-white transition-colors hover:bg-blue-600 disabled:cursor-not-allowed disabled:opacity-40"
                >
                  {sending ? <ProgressActivityIcon className="h-4 w-4 animate-spin" /> : <SendIcon className="h-4 w-4" />}
                </button>
              </div>
              <div className="mt-1.5 flex items-center justify-between gap-3 text-[10px] text-slate-500">
                <span>Enter to send · Shift+Enter for a new line</span>
                <span className="t-num">{draft.length}/{MESSAGE_LIMIT}</span>
              </div>
            </div>
          </>
        )}
      </section>
    </div>
  )
}

function DisclosureGate({ onAcknowledge }: { onAcknowledge: () => void }) {
  return (
    <div className="flex min-h-full flex-1 items-center justify-center px-4 py-8" aria-label="AI support disclosure">
      <div className="w-full max-w-md rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 text-center shadow-2xl shadow-black/20 sm:p-6">
        <span className="halo-amber mx-auto flex h-10 w-10 items-center justify-center rounded-xl">
          <GppMaybeIcon className="h-5 w-5" />
        </span>
        <h2 className="mt-3 text-base font-semibold text-slate-100">Before you chat</h2>
        <p className="mt-2 text-xs leading-relaxed text-slate-400">
          This is an online AI assistant. Messages, product/version context, SBC model, selected non-secret support settings, and the Pi connection's public IP for abuse prevention are sent to Sentry Six; relevant content is processed by Ollama Cloud. The support settings are limited to Dashcam Size, Travel Mode flags, archive category toggles, and whether Wraps and Lock Chimes are enabled. Redacted chats may be kept for up to 90 days after the last activity and reviewed by authorized maintainers.
        </p>
        <p className="mt-2 text-xs leading-relaxed text-slate-500">
          AI can be wrong and cannot approve refunds or commitments. Do not share secrets, and use a trusted local network because your browser may connect to the Pi over HTTP.
        </p>
        <p className="mt-2 text-xs leading-relaxed text-slate-500">
          Files require separate approval. Discord is optional and this chat is never transferred there automatically.
        </p>
        <p className="mt-2 text-xs leading-relaxed text-slate-500">
          AI may offer local navigation or reviewed setting buttons. Nothing changes unless you click; settings show a separate confirmation and may require a confirmed Pi restart.
        </p>
        <button
          type="button"
          onClick={onAcknowledge}
          className="mt-4 w-full rounded-xl bg-blue-500 px-4 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-blue-600"
        >
          Acknowledge &amp; continue
        </button>
        <a
          href="https://sentry-six.com/privacy"
          target="_blank"
          rel="noopener noreferrer"
          className="mt-3 inline-flex text-[11px] text-blue-400 hover:text-blue-300"
        >
          Privacy policy <OpenInNewIcon className="ml-1 h-3 w-3" />
        </a>
      </div>
    </div>
  )
}

function DisclosureRefreshGate({
  deleting,
  error,
  onDelete,
  onForget,
}: {
  deleting: boolean
  error: string | null
  onDelete: () => void
  onForget: () => void
}) {
  return (
    <div className="flex min-h-full flex-1 items-center justify-center px-4 py-8" aria-label="AI support privacy update">
      <div className="w-full max-w-md rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 text-center shadow-2xl shadow-black/20 sm:p-6">
        <span className="halo-amber mx-auto flex h-10 w-10 items-center justify-center rounded-xl">
          <GppMaybeIcon className="h-5 w-5" />
        </span>
        <h2 className="mt-3 text-base font-semibold text-slate-100">AI Support privacy update</h2>
        <p className="mt-2 text-xs leading-relaxed text-slate-400">
          Your saved chat used an earlier disclosure. To protect your choice, it cannot continue under changed terms.
        </p>
        <p className="mt-2 text-xs leading-relaxed text-slate-500">
          Deleting it permanently removes its server transcript and diagnostics, then shows the current disclosure before a new chat starts.
        </p>
        {error && <p className="mt-3 text-xs text-rose-300" role="alert">{error}</p>}
        <button
          type="button"
          onClick={onDelete}
          disabled={deleting}
          className="mt-4 flex w-full items-center justify-center gap-2 rounded-xl bg-rose-500/85 px-4 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-rose-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {deleting && <ProgressActivityIcon className="h-4 w-4 animate-spin" />}
          Delete old chat &amp; continue
        </button>
        {error && (
          <button
            type="button"
            onClick={onForget}
            disabled={deleting}
            className="mt-2 w-full rounded-xl border border-white/10 px-4 py-2 text-xs font-medium text-slate-400 transition-colors hover:bg-white/[0.05] hover:text-slate-200 disabled:opacity-50"
          >
            Forget local access without verified deletion
          </button>
        )}
        <a
          href="https://sentry-six.com/privacy"
          target="_blank"
          rel="noopener noreferrer"
          className="mt-3 inline-flex text-[11px] text-blue-400 hover:text-blue-300"
        >
          Privacy policy <OpenInNewIcon className="ml-1 h-3 w-3" />
        </a>
      </div>
    </div>
  )
}

function EmptyConversation({ onSuggestion }: { onSuggestion: (message: string) => void }) {
  const suggestions = [
    "Why is my Pi showing as offline?",
    "How do I check whether archiving is working?",
    "Help me understand a Sentry USB setting.",
  ]
  return (
    <div className="flex min-h-full flex-col items-center justify-center py-6 text-center">
      <div className="halo-accent mb-3 flex h-14 w-14 items-center justify-center rounded-2xl">
        <SmartToyIcon className="h-7 w-7" />
      </div>
      <h2 className="text-base font-semibold text-slate-200">What can I help with?</h2>
      <p className="mt-1 max-w-lg text-xs leading-relaxed text-slate-500">
        This assistant is limited to Sentry USB Rusty help. It can explain setup and features, inspect approved diagnostics with your permission, and offer reviewed local navigation or setting actions.
      </p>
      <div className="mt-4 flex max-w-2xl flex-wrap justify-center gap-2">
        {suggestions.map((suggestion) => (
          <button
            key={suggestion}
            type="button"
            onClick={() => onSuggestion(suggestion)}
            className="rounded-xl border border-white/10 bg-white/[0.025] px-3 py-2 text-xs text-slate-400 transition-colors hover:bg-white/[0.06] hover:text-slate-200"
          >
            {suggestion}
          </button>
        ))}
      </div>
    </div>
  )
}

function MessageBubble({
  message,
  fileDecisions,
  onFileDecision,
  uiActionStates,
  onUiAction,
}: {
  message: SupportMessage
  fileDecisions: Record<string, FileDecisionViewState>
  onFileDecision: (reference: FileRequestReference, decision: "approved" | "denied") => Promise<void>
  uiActionStates: Record<string, UiActionViewState>
  onUiAction: (messageId: string, action: SupportUiAction) => Promise<void>
}) {
  if (message.role === "system") {
    return (
      <div className="my-3 flex justify-center">
        <div className="max-w-2xl rounded-full border border-white/[0.07] bg-white/[0.025] px-3 py-1 text-center text-[10px] text-slate-500">
          {message.content}
        </div>
      </div>
    )
  }

  const isUser = message.role === "user"
  return (
    <article className={cn("ai-message-enter mb-4 flex items-end gap-2", isUser ? "justify-end" : "justify-start")}>
      {!isUser && (
        <div className="ai-assistant-avatar halo-accent flex h-8 w-8 shrink-0 items-center justify-center rounded-xl" aria-hidden="true">
          <SmartToyIcon className="h-4 w-4" />
        </div>
      )}
      <div className={cn("max-w-[88%] sm:max-w-[76%]", !isUser && "min-w-0")}>
        {!isUser && <p className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-blue-400">Sentry AI</p>}
        <div className={cn(
          "rounded-2xl px-3.5 py-2.5 transition-colors duration-300",
          isUser
            ? "ai-bubble-user rounded-br-md text-slate-100"
            : "ai-bubble-assistant rounded-bl-md text-slate-300",
        )}>
          {isUser
            ? <p className="whitespace-pre-wrap break-words text-sm leading-relaxed">{message.content}</p>
            : <AssistantMarkdown content={message.content} />}
          <p className={cn("mt-1 text-[10px]", isUser ? "text-blue-200/50" : "text-slate-600")}>{displayTime(message.createdAt)}</p>
        </div>

        {message.actions?.fileRequests?.map((request, index) => {
          const reference = { messageId: message.id, requestIndex: index, request }
          const key = `${message.id}:${index}`
          return (
            <FileConsentCard
              key={key}
              reference={reference}
              state={fileDecisions[key] ?? { phase: "idle" }}
              onDecision={onFileDecision}
            />
          )
        })}

        {message.actions?.uiActions?.map((action) => {
          const key = `${message.id}:${action.id}`
          return (
            <UiActionCard
              key={key}
              action={action}
              state={uiActionStates[key] ?? { phase: "idle" }}
              onActivate={() => onUiAction(message.id, action)}
            />
          )
        })}

        {message.actions?.suggestDiscord && <DiscordSuggestion />}
      </div>
    </article>
  )
}

function UiActionCard({
  action,
  state,
  onActivate,
}: {
  action: SupportUiAction
  state: UiActionViewState
  onActivate: () => Promise<void>
}) {
  const local = localUiAction(action)
  const isSetting = local?.kind === "setting"
  const isOperation = local?.kind === "operation"
  const [rebootArmed, setRebootArmed] = useState(false)
  const [rebooting, setRebooting] = useState(false)
  const [rebootError, setRebootError] = useState<string | null>(null)

  async function handleReboot() {
    if (!rebootArmed) {
      setRebootArmed(true)
      window.setTimeout(() => setRebootArmed(false), 10000)
      return
    }
    setRebooting(true)
    setRebootError(null)
    try {
      const response = await fetch("/api/system/reboot", { method: "POST" })
      if (!response.ok) throw new Error(`Restart failed (${response.status})`)
    } catch (error) {
      setRebooting(false)
      setRebootArmed(false)
      setRebootError(errorMessage(error, "The Pi could not be restarted."))
    }
  }
  return (
    <section className={cn(
      "ai-permission-card mt-2.5 rounded-xl px-3 py-2.5",
      isSetting ? "ai-permission-setting" : isOperation ? "ai-permission-operation" : "ai-permission-navigation",
    )}>
      <p className={cn(
        "text-xs font-semibold",
        isSetting ? "text-violet-200" : isOperation ? "text-teal-200" : "text-sky-200",
      )}>
        {isSetting ? "Suggested setting change" : isOperation ? "Suggested local action" : "Open this setting"}
      </p>
      <p className="mt-0.5 text-[11px] text-slate-400">{local?.label || "Unsupported action"}</p>
      {state.phase === "success" ? (
        <div className="mt-2 space-y-2">
          <p className="flex items-start gap-2 text-xs text-emerald-300" role="status">
            <TaskIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />{state.detail}
          </p>
          {state.requiresReboot && (
            <button
              type="button"
              disabled={rebooting}
              onClick={() => { void handleReboot() }}
              className="inline-flex items-center gap-1.5 rounded-lg border border-amber-400/20 bg-amber-500/10 px-3 py-1.5 text-xs font-medium text-amber-200 hover:bg-amber-500/20 disabled:opacity-50"
            >
              {rebooting && <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />}
              {rebooting ? "Restarting…" : rebootArmed ? "Confirm Restart" : "Restart Pi"}
            </button>
          )}
          {rebootError && <p className="text-xs text-rose-300" role="alert">{rebootError}</p>}
        </div>
      ) : state.phase === "error" ? (
        <p className="mt-2 flex items-start gap-2 text-xs text-rose-300" role="alert">
          <ErrorIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />{state.detail}
        </p>
      ) : (
        <button
          type="button"
          disabled={!local || state.phase === "working"}
          onClick={() => { void onActivate() }}
          className={cn(
            "mt-2 inline-flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-xs font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-50",
            isSetting
              ? "bg-violet-500/15 text-violet-200 hover:bg-violet-500/25"
              : isOperation
                ? "bg-teal-500/15 text-teal-200 hover:bg-teal-500/25"
                : "bg-sky-500/15 text-sky-200 hover:bg-sky-500/25",
          )}
        >
          {state.phase === "working" ? (
            <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
          ) : isSetting || isOperation ? (
            <CheckIcon className="h-3.5 w-3.5" />
          ) : (
            <OpenInNewIcon className="h-3.5 w-3.5" />
          )}
          {state.phase === "working" ? (isOperation ? "Running…" : "Applying…") : local?.label || "Unavailable"}
        </button>
      )}
    </section>
  )
}

function TypingBubble() {
  return (
    <div className="ai-message-enter mb-4 flex items-end gap-2" role="status" aria-live="polite" aria-label="Sentry AI is preparing a response">
      <div className="ai-assistant-avatar halo-accent flex h-8 w-8 shrink-0 items-center justify-center rounded-xl" aria-hidden="true">
        <SmartToyIcon className="h-4 w-4" />
      </div>
      <div className="ai-bubble-assistant ai-typing-shimmer rounded-2xl rounded-bl-md px-4 py-3" aria-hidden="true">
        <div className="flex items-center gap-1.5">
          {[0, 1, 2].map((dot) => (
            <span
              key={dot}
              className="h-1.5 w-1.5 animate-bounce rounded-full bg-slate-400 motion-reduce:animate-none"
              style={{ animationDelay: `${dot * 140}ms`, animationDuration: "900ms" }}
            />
          ))}
        </div>
      </div>
      <span className="sr-only">Sentry AI is preparing a response.</span>
    </div>
  )
}

function FileConsentCard({
  reference,
  state,
  onDecision,
}: {
  reference: FileRequestReference
  state: FileDecisionViewState
  onDecision: (reference: FileRequestReference, decision: "approved" | "denied") => Promise<void>
}) {
  const { request } = reference
  const policyIssue = filePolicyIssue(request)
  const working = state.phase === "approving" || state.phase === "denying"

  return (
    <section className="ai-permission-card ai-permission-file mt-2.5 overflow-hidden rounded-xl" aria-label={`File access request for ${request.label}`}>
      <div className="flex items-start gap-2.5 px-3 py-2.5">
        <span className="halo-amber flex h-8 w-8 shrink-0 items-center justify-center rounded-lg">
          <EncryptedIcon className="h-4 w-4" />
        </span>
        <div className="min-w-0 flex-1">
          <p className="text-xs font-semibold text-amber-100">AI requests one-time access</p>
          <p className="mt-0.5 text-xs text-slate-300">{request.label}</p>
          <p className="mt-1 text-[11px] leading-relaxed text-slate-500">{request.reason}</p>
        </div>
      </div>
      <dl className="grid gap-x-3 gap-y-1 border-y border-white/[0.06] bg-black/10 px-3 py-2 text-[10px] sm:grid-cols-[100px_1fr]">
        <dt className="text-slate-500">File</dt><dd className="break-all text-slate-300">{request.fileName} · includes BLE telemetry and recent archiveloop.log · max {formatBytes(Math.min(request.maxBytes, LOCAL_DIAGNOSTIC_HARD_LIMIT))}</dd>
        <dt className="text-slate-500">Destination</dt><dd className="text-slate-300">api.sentry-six.com · Sentry Six support servers</dd>
        <dt className="text-slate-500">AI processing</dt><dd className="text-slate-300">May be processed by Ollama Cloud for this answer</dd>
        <dt className="text-slate-500">Retention</dt><dd className="text-slate-300">Deleted within 7 days</dd>
      </dl>
      <div className="px-3 py-2.5">
        <p className="mb-2 text-[10px] leading-relaxed text-slate-500">
        Includes date, hostname, uptime, software/OS/board and service details, storage, USB-gadget and network status, temperatures, Bluetooth adapter and BLE telemetry state, and recent logs including archiveloop, the drive-import history and the Sentry USB service journal. Logs can incidentally contain local IPs, device or vehicle identifiers, error payloads, or location-related details. Do not upload secrets or data you are not authorized to share. Nothing is collected or uploaded until you approve.
        </p>
        {policyIssue && state.phase === "idle" && (
          <p className="mb-2 flex items-start gap-1.5 text-[10px] text-rose-300"><ErrorIcon className="mt-0.5 h-3 w-3 shrink-0" />{policyIssue}</p>
        )}

        {state.phase === "idle" ? (
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              disabled={Boolean(policyIssue)}
              onClick={() => { void onDecision(reference, "approved") }}
              className="flex items-center gap-1.5 rounded-lg bg-amber-400/15 px-3 py-1.5 text-xs font-medium text-amber-200 hover:bg-amber-400/25 disabled:cursor-not-allowed disabled:opacity-40"
            >
              <CheckIcon className="h-3.5 w-3.5" /> Generate &amp; upload diagnostics once
            </button>
            <button
              type="button"
              onClick={() => { void onDecision(reference, "denied") }}
              className="flex items-center gap-1.5 rounded-lg border border-white/10 px-3 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06]"
            >
              <CloseIcon className="h-3.5 w-3.5" /> Deny
            </button>
          </div>
        ) : working ? (
          <p className="flex items-center gap-2 text-xs text-amber-200" role="status">
            <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
            {state.phase === "approving" ? "Preparing and sharing approved diagnostics…" : "Saving your decision…"}
          </p>
        ) : state.phase === "approved" ? (
          <p className="flex items-start gap-2 text-xs text-emerald-300"><TaskIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />{state.detail || "Diagnostics shared once."}</p>
        ) : state.phase === "denied" ? (
          <p className="flex items-start gap-2 text-xs text-slate-400"><CloseIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />Denied. No file was collected or uploaded.</p>
        ) : (
          <div className="space-y-2">
            <p className="flex items-start gap-2 text-xs text-rose-300" role="alert"><ErrorIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />{state.detail || "The file was not shared."}</p>
            {state.retryDecision && (
              <button
                type="button"
                onClick={() => { void onDecision(reference, state.retryDecision as "approved" | "denied") }}
                className="rounded-lg border border-white/10 px-3 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06]"
              >
                {state.retryDecision === "approved" ? "Try upload again" : "Try deny again"}
              </button>
            )}
          </div>
        )}
      </div>
    </section>
  )
}

function DiscordSuggestion() {
  return (
    <div className="mt-2.5 rounded-xl border border-violet-400/20 bg-violet-500/[0.07] px-3 py-2.5">
      <div className="flex items-start gap-2.5">
        <GroupIcon className="mt-0.5 h-4 w-4 shrink-0 text-violet-300" />
        <div className="min-w-0 flex-1">
          <p className="text-xs font-semibold text-violet-200">Want help from a person?</p>
          <p className="mt-0.5 text-[11px] leading-relaxed text-slate-400">Join the Sentry Six Discord community. This AI conversation is not transferred—share only what you choose.</p>
          <DiscordLink />
        </div>
      </div>
    </div>
  )
}

function DiscordLink({ compact = false }: { compact?: boolean }) {
  return (
    <a
      href={DISCORD_URL}
      target="_blank"
      rel="noopener noreferrer"
      className={cn(
        "inline-flex items-center gap-1.5 rounded-lg font-medium text-violet-300 transition-colors hover:bg-violet-500/15 hover:text-violet-200",
        compact ? "border border-violet-400/15 px-2.5 py-1.5 text-xs" : "mt-2 bg-violet-500/10 px-3 py-1.5 text-xs",
      )}
    >
      <GroupIcon className="h-3.5 w-3.5" /> Join Discord <OpenInNewIcon className="h-3 w-3" />
    </a>
  )
}
