import { useState } from "react"
import { Shield, LogIn, Loader2, AlertCircle } from "lucide-react"
import { useAuth } from "@/hooks/useAuth"

interface LoginProps {
  onLogin?: (username: string, password: string) => Promise<string | null>
}

export default function Login({ onLogin }: LoginProps) {
  const { login: contextLogin } = useAuth()
  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState("")

  const doLogin = onLogin || contextLogin

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault()
    if (!username.trim() || !password) return

    setLoading(true)
    setError("")
    const err = await doLogin(username.trim(), password)
    if (err) {
      setError(err)
      setLoading(false)
    }
  }

  return (
    <div className="flex h-screen items-center justify-center bg-slate-950">
      <div className="glass-card w-full max-w-sm p-6">
        <div className="mb-6 flex flex-col items-center">
          <div className="mb-3 flex h-14 w-14 items-center justify-center rounded-full bg-blue-500/15">
            <Shield className="h-7 w-7 text-blue-400" />
          </div>
          <h1 className="text-lg font-semibold text-slate-100">Sentry USB</h1>
          <p className="mt-1 text-center text-sm text-slate-500">
            Sign in to access the dashboard.
          </p>
        </div>

        {error && (
          <div className="mb-4 flex items-center gap-2 rounded-lg bg-red-500/10 px-3 py-2 text-sm text-red-400">
            <AlertCircle className="h-4 w-4 shrink-0" />
            {error}
          </div>
        )}

        <form onSubmit={handleSubmit} className="space-y-3">
          <div>
            <label htmlFor="username" className="mb-1 block text-xs font-medium text-slate-400">Username</label>
            <input
              id="username"
              name="username"
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="Username"
              autoFocus
              autoComplete="username"
              className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2.5 text-sm text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
            />
          </div>
          <div>
            <label htmlFor="password" className="mb-1 block text-xs font-medium text-slate-400">Password</label>
            <input
              id="password"
              name="password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="Password"
              autoComplete="current-password"
              className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2.5 text-sm text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
            />
          </div>
          <button
            type="submit"
            disabled={loading || !username.trim() || !password}
            className="flex w-full items-center justify-center gap-2 rounded-lg bg-blue-500 px-4 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-600 disabled:opacity-50"
          >
            {loading ? (
              <>
                <Loader2 className="h-4 w-4 animate-spin" />
                Signing in...
              </>
            ) : (
              <>
                <LogIn className="h-4 w-4" />
                Sign In
              </>
            )}
          </button>
        </form>
      </div>
    </div>
  )
}
