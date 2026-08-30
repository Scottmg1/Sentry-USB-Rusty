import { useMemo } from "react"
import type {
  FileRequestReference,
  SupportConversation,
  SupportMessage,
  SupportTransport,
} from "@/api/support"
import { AISupportChat } from "@/components/support/AISupportChat"

const CONVERSATION: SupportConversation = {
  id: "support-preview",
  authToken: "preview-only",
}

function previewMessages(userMessage: string): SupportMessage[] {
  const now = new Date().toISOString()
  return [
    { id: "preview-user", role: "user", content: userMessage, createdAt: now },
    {
      id: "preview-navigation",
      role: "assistant",
      content: "I can take you directly to the **Archive** step in Settings so you can review the current destination and clip categories.",
      createdAt: now,
      actions: {
        uiActions: [{ id: "open-archive-settings", kind: "navigate", label: "Open Archive settings" }],
      },
    },
    {
      id: "preview-setting",
      role: "assistant",
      content: "If you want, I can enable **Sentry Clips** and disable **Recent Clips** while leaving Saved Clips and Track Mode unchanged. You will confirm the exact change locally before anything is written.",
      createdAt: now,
      actions: {
        uiActions: [{
          id: "set-archive-sentry-without-recent",
          kind: "setting",
          label: "Use Sentry Clips, not Recent Clips",
        }],
      },
    },
    {
      id: "preview-operation",
      role: "assistant",
      content: "You can also start the same **Archive Sync** action available in Settings. This requires a separate confirmation.",
      createdAt: now,
      actions: {
        uiActions: [{ id: "run-archive-sync", kind: "operation", label: "Run Archive Sync" }],
      },
    },
    {
      id: "preview-diagnostic",
      role: "assistant",
      content: "If archiving still fails, the approved Diagnostics report would help distinguish configuration, reachability, and service errors.",
      createdAt: now,
      actions: {
        fileRequests: [{
          kind: "diagnostics",
          label: "Share Diagnostics report",
          fileName: "sentryusb-diagnostics-with-archiveloop.txt",
          reason: "Review the bounded diagnostic evidence for this archive problem.",
          maxBytes: 2 * 1024 * 1024,
          retentionDays: 7,
        }],
      },
    },
  ]
}

export default function PreviewSupport() {
  const transport = useMemo<SupportTransport>(() => {
    return {
      async createConversation(message) {
        const messages = previewMessages(message)
        return { conversation: CONVERSATION, messages, status: "idle" }
      },
      async fetchMessages() {
        return { messages: previewMessages("Show me how to control what gets archived."), status: "idle" }
      },
      async sendMessage(_conversation, content) {
        const messages = previewMessages(content)
        return { messages, status: "idle" }
      },
      async deleteConversation() {},
      async decideFileRequest(
        _conversation: SupportConversation,
        _reference: FileRequestReference,
        decision: "approved" | "denied",
      ) {
        return decision === "approved"
          ? { decision, uploadToken: "preview-upload", maxBytes: 2 * 1024 * 1024 }
          : { decision }
      },
      async collectDiagnostics() {
        return new Blob(["preview diagnostics"], { type: "text/plain" })
      },
      async uploadRequestedFile() {
        return {
          fileId: "preview-file",
          fileName: "sentryusb-diagnostics-with-archiveloop.txt",
          size: 19,
          retentionDays: 7,
        }
      },
    }
  }, [])

  return (
    <div className="min-h-screen bg-slate-950 p-4 text-slate-100 sm:p-6">
      <div className="mx-auto max-w-5xl">
        <p className="mb-3 text-xs text-slate-500">
          Dev-only Sentry AI visual preview. Actions use mock conversation data; local action endpoints remain unchanged.
        </p>
        <AISupportChat transport={transport} storageKey={null} />
      </div>
    </div>
  )
}
