export const SUPPORT_DISCLOSURE_VERSION = "rusty-ai-support-2026-08-05"
export const DIAGNOSTIC_CONSENT_VERSION = "diagnostic-upload-v1"

const SUPPORT_API = "/api/support/ai"
const LOCAL_DIAGNOSTICS_MAX_BYTES = 2 * 1024 * 1024

export type SupportMessageRole = "user" | "assistant" | "system"
export type ConversationStatus = "idle" | "generating" | "closed"

export interface SupportFileRequest {
  kind: string
  label: string
  fileName: string
  reason: string
  maxBytes: number
  retentionDays: number
}

export interface SupportMessageActions {
  fileRequests?: SupportFileRequest[]
  suggestDiscord?: boolean
}

export interface SupportMessage {
  id: string
  role: SupportMessageRole
  content: string
  createdAt: string
  actions?: SupportMessageActions
}

export interface SupportConversation {
  id: string
  authToken: string
}

export interface ConversationResult {
  conversation: SupportConversation
  messages: SupportMessage[]
  status: ConversationStatus
}

export interface MessagesResult {
  messages: SupportMessage[]
  status: ConversationStatus
}

export interface FileRequestReference {
  messageId: string
  requestIndex: number
  request: SupportFileRequest
}

export interface FileDecisionResult {
  decision: "approved" | "denied"
  uploadToken?: string
  maxBytes?: number
  expiresAt?: string
}

export interface FileUploadResult {
  fileId?: string
  fileName: string
  size: number
  retentionDays: number
}

export interface SupportTransport {
  createConversation(message: string, clientMessageId: string): Promise<ConversationResult>
  fetchMessages(conversation: SupportConversation): Promise<MessagesResult>
  sendMessage(
    conversation: SupportConversation,
    content: string,
    clientMessageId: string,
  ): Promise<MessagesResult>
  deleteConversation(conversation: SupportConversation): Promise<void>
  decideFileRequest(
    conversation: SupportConversation,
    reference: FileRequestReference,
    decision: "approved" | "denied",
  ): Promise<FileDecisionResult>
  collectDiagnostics(maxBytes: number): Promise<Blob>
  uploadRequestedFile(
    conversation: SupportConversation,
    reference: FileRequestReference,
    uploadToken: string,
    file: Blob,
  ): Promise<FileUploadResult>
}

type JsonRecord = Record<string, unknown>

export class SupportApiError extends Error {
  readonly status: number
  readonly code?: string

  constructor(message: string, status: number, code?: string) {
    super(message)
    this.name = "SupportApiError"
    this.status = status
    this.code = code
  }
}

export type SupportConversationResetReason = "unavailable" | "disclosure"

export function isSupportConversationGone(error: unknown) {
  return error instanceof SupportApiError && [404, 410].includes(error.status)
}

export function supportConversationResetReason(error: unknown): SupportConversationResetReason | null {
  if (!(error instanceof SupportApiError)) return null
  if ([401, 404, 410].includes(error.status)) return "unavailable"
  if (
    error.status === 409
    && (error.code === "disclosure_required" || /disclosure/i.test(error.message))
  ) return "disclosure"
  return null
}

async function errorFromResponse(response: Response, fallback: string): Promise<Error> {
  const data = await response.json().catch(() => null) as JsonRecord | null
  const message = typeof data?.error === "string" ? data.error : `${fallback} (${response.status})`
  const code = typeof data?.code === "string" ? data.code : undefined
  return new SupportApiError(message, response.status, code)
}

async function jsonRequest<T>(url: string, init: RequestInit, fallback: string): Promise<T> {
  const response = await fetch(url, init)
  if (!response.ok) throw await errorFromResponse(response, fallback)
  return response.json() as Promise<T>
}

function authHeaders(conversation: SupportConversation, includeJson = true): HeadersInit {
  return {
    ...(includeJson ? { "Content-Type": "application/json" } : {}),
    "X-Auth-Token": conversation.authToken,
  }
}

function normalizedStatus(value: unknown): ConversationStatus {
  return value === "generating" || value === "closed" ? value : "idle"
}

function normalizedMessages(value: unknown): SupportMessage[] {
  return Array.isArray(value) ? value as SupportMessage[] : []
}

interface ConversationPayload {
  success?: boolean
  id?: string
  authToken?: string
  messages?: SupportMessage[]
  status?: ConversationStatus
}

interface MessagesPayload {
  success?: boolean
  messages?: SupportMessage[]
  status?: ConversationStatus
}

interface FileDecisionPayload {
  success?: boolean
  decision?: "approved" | "denied"
  uploadToken?: string
  maxBytes?: number
  expiresAt?: string
}

interface FileUploadPayload {
  success?: boolean
  fileId?: string
  fileName?: string
  size?: number
  retentionDays?: number
}

export const realSupportTransport: SupportTransport = {
  async createConversation(message, clientMessageId) {
    const data = await jsonRequest<ConversationPayload>(
      `${SUPPORT_API}/conversations`,
      {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Idempotency-Key": clientMessageId,
        },
        body: JSON.stringify({
          message,
          clientMessageId,
          disclosureAccepted: true,
          disclosureVersion: SUPPORT_DISCLOSURE_VERSION,
        }),
      },
      "Could not start the AI support conversation",
    )

    if (!data.id || !data.authToken) {
      throw new Error("The support server returned an incomplete conversation")
    }

    return {
      conversation: { id: data.id, authToken: data.authToken },
      messages: normalizedMessages(data.messages),
      status: normalizedStatus(data.status),
    }
  },

  async fetchMessages(conversation) {
    const data = await jsonRequest<MessagesPayload>(
      `${SUPPORT_API}/conversations/${encodeURIComponent(conversation.id)}/messages`,
      { headers: authHeaders(conversation, false), cache: "no-store" },
      "Could not refresh the conversation",
    )
    return {
      messages: normalizedMessages(data.messages),
      status: normalizedStatus(data.status),
    }
  },

  async sendMessage(conversation, content, clientMessageId) {
    const data = await jsonRequest<MessagesPayload>(
      `${SUPPORT_API}/conversations/${encodeURIComponent(conversation.id)}/messages`,
      {
        method: "POST",
        headers: {
          ...authHeaders(conversation),
          "Idempotency-Key": clientMessageId,
        },
        body: JSON.stringify({ content, clientMessageId }),
      },
      "Could not send the message",
    )
    return {
      messages: normalizedMessages(data.messages),
      status: normalizedStatus(data.status),
    }
  },

  async deleteConversation(conversation) {
    await jsonRequest<JsonRecord>(
      `${SUPPORT_API}/conversations/${encodeURIComponent(conversation.id)}`,
      {
        method: "DELETE",
        headers: {
          ...authHeaders(conversation, false),
          "Idempotency-Key": `delete-${conversation.id}`,
        },
      },
      "Could not end the conversation",
    )
  },

  async decideFileRequest(conversation, reference, decision) {
    const data = await jsonRequest<FileDecisionPayload>(
      `${SUPPORT_API}/conversations/${encodeURIComponent(conversation.id)}/file-decisions`,
      {
        method: "POST",
        headers: {
          ...authHeaders(conversation),
          "Idempotency-Key": `decision-${reference.messageId}-${reference.requestIndex}-${decision}`,
        },
        body: JSON.stringify({
          messageId: reference.messageId,
          requestIndex: reference.requestIndex,
          decision,
          consentVersion: DIAGNOSTIC_CONSENT_VERSION,
        }),
      },
      "Could not save the file decision",
    )

    if (decision === "approved" && !data.uploadToken) {
      throw new Error("The support server did not provide a one-time upload token")
    }

    return {
      decision,
      uploadToken: data.uploadToken,
      maxBytes: data.maxBytes,
      expiresAt: data.expiresAt,
    }
  },

  async collectDiagnostics(maxBytes) {
    const enforcedMax = Math.min(maxBytes, LOCAL_DIAGNOSTICS_MAX_BYTES)
    if (!Number.isFinite(enforcedMax) || enforcedMax <= 0) {
      throw new Error("The requested diagnostic size limit is invalid")
    }

    const refresh = await fetch("/api/diagnostics/refresh", { method: "POST" })
    if (!refresh.ok) throw await errorFromResponse(refresh, "Could not prepare diagnostics")

    const response = await fetch("/api/diagnostics", { cache: "no-store" })
    if (!response.ok) throw await errorFromResponse(response, "Could not read diagnostics")

    const declaredSize = Number(response.headers.get("content-length"))
    if (Number.isFinite(declaredSize) && declaredSize > enforcedMax) {
      throw new Error(`Diagnostics exceed the approved ${formatByteLimit(enforcedMax)} limit`)
    }

    const bytes = await response.arrayBuffer()
    if (bytes.byteLength > enforcedMax) {
      throw new Error(`Diagnostics exceed the approved ${formatByteLimit(enforcedMax)} limit`)
    }
    return new Blob([bytes], { type: "text/plain" })
  },

  async uploadRequestedFile(conversation, reference, uploadToken, file) {
    const form = new FormData()
    form.append("uploadToken", uploadToken)
    form.append("messageId", reference.messageId)
    form.append("requestIndex", String(reference.requestIndex))
    form.append("kind", "diagnostics")
    form.append("consentVersion", DIAGNOSTIC_CONSENT_VERSION)
    form.append("file", file, reference.request.fileName)

    const data = await jsonRequest<FileUploadPayload>(
      `${SUPPORT_API}/conversations/${encodeURIComponent(conversation.id)}/files`,
      {
        method: "POST",
        headers: {
          ...authHeaders(conversation, false),
          "Idempotency-Key": `upload-${reference.messageId}-${reference.requestIndex}`,
        },
        body: form,
      },
      "Could not upload the approved diagnostics",
    )

    return {
      fileId: data.fileId,
      fileName: data.fileName || reference.request.fileName,
      size: typeof data.size === "number" ? data.size : file.size,
      retentionDays: typeof data.retentionDays === "number" ? data.retentionDays : 7,
    }
  },
}

function formatByteLimit(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  return `${Math.ceil(bytes / 1024)} KB`
}
