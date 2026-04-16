import { createContext, useContext, useState, useEffect, useCallback, type ReactNode } from "react"

type AuthState = "loading" | "authenticated" | "unauthenticated"

interface AuthContextValue {
  state: AuthState
  authRequired: boolean
  login: (username: string, password: string) => Promise<string | null>
  logout: () => Promise<void>
}

const AuthContext = createContext<AuthContextValue>({
  state: "loading",
  authRequired: false,
  login: async () => null,
  logout: async () => {},
})

export function useAuth() {
  return useContext(AuthContext)
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AuthState>("loading")
  const [authRequired, setAuthRequired] = useState(false)

  const checkAuth = useCallback(async () => {
    try {
      const res = await fetch("/api/auth/check")
      const data = await res.json()
      setAuthRequired(data.auth_required)
      setState(data.authenticated || !data.auth_required ? "authenticated" : "unauthenticated")
    } catch {
      // If check fails (e.g., server down), assume authenticated to avoid blocking
      setState("authenticated")
    }
  }, [])

  async function login(username: string, password: string): Promise<string | null> {
    try {
      const res = await fetch("/api/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password }),
      })
      if (res.ok) {
        setState("authenticated")
        return null
      }
      const data = await res.json()
      return data.error || "Invalid username or password"
    } catch {
      return "Connection failed. Please try again."
    }
  }

  async function logout() {
    await fetch("/api/auth/logout", { method: "POST" }).catch(() => {})
    setState("unauthenticated")
  }

  useEffect(() => {
    checkAuth()
    // Re-check periodically so we detect invalidated sessions (e.g., after
    // server restart which clears in-memory sessions).
    const iv = setInterval(checkAuth, 10_000)
    return () => clearInterval(iv)
  }, [checkAuth])

  return (
    <AuthContext.Provider value={{ state, authRequired, login, logout }}>
      {children}
    </AuthContext.Provider>
  )
}
