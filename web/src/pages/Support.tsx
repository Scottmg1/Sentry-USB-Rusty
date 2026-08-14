import { realSupportTransport } from "@/api/support"
import { AISupportChat } from "@/components/support/AISupportChat"

export default function Support() {
  return <AISupportChat transport={realSupportTransport} />
}
