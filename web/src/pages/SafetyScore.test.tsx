import assert from "node:assert/strict"
import test from "node:test"

import { act, createElement } from "react"
import { MemoryRouter } from "react-router-dom"
import { Window } from "happy-dom"

import SafetyScore from "./SafetyScore.tsx"

test("All remains active and requested after leaving and reopening Safety Score", async () => {
  const testWindow = new Window({ url: "http://localhost/" })
  testWindow.document.body.innerHTML = "<div id='root'></div>"
  const requests: string[] = []
  const originalFetch = globalThis.fetch
  const globals = globalThis as typeof globalThis & {
    IS_REACT_ACT_ENVIRONMENT?: boolean
  }
  const originalActEnvironment = globals.IS_REACT_ACT_ENVIRONMENT
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window")
  const originalDocument = Object.getOwnPropertyDescriptor(globalThis, "document")
  const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, "navigator")
  globals.IS_REACT_ACT_ENVIRONMENT = true

  Object.defineProperty(globalThis, "window", { configurable: true, value: testWindow })
  Object.defineProperty(globalThis, "document", { configurable: true, value: testWindow.document })
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: testWindow.navigator })
  globalThis.fetch = async (input) => {
    requests.push(String(input))
    const body = String(input) === "/api/drives" ? [] : {}
    return new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    })
  }

  const { createRoot } = await import("react-dom/client")
  const container = testWindow.document.querySelector<HTMLDivElement>("#root")!
  let root = createRoot(container)
  try {
    await act(async () => {
      root.render(createElement(MemoryRouter, null, createElement(SafetyScore)))
    })

    const allButton = Array.from(container.querySelectorAll("button")).find(
      (button) => button.textContent === "All",
    )
    assert.ok(allButton)
    await act(async () => {
      allButton.click()
    })
    assert.ok(requests.includes("/api/drives/safety-analytics?period=all"))

    await act(async () => root.unmount())
    requests.length = 0
    root = createRoot(container)
    await act(async () => {
      root.render(createElement(MemoryRouter, null, createElement(SafetyScore)))
    })

    const reopenedAllButton = Array.from(container.querySelectorAll("button")).find(
      (button) => button.textContent === "All",
    )
    assert.ok(reopenedAllButton)
    assert.match(reopenedAllButton.className, /bg-white\/10/)
    assert.ok(requests.includes("/api/drives/safety-analytics?period=all"))
  } finally {
    await act(async () => root.unmount())
    globalThis.fetch = originalFetch
    if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow)
    else Reflect.deleteProperty(globalThis, "window")
    if (originalDocument) Object.defineProperty(globalThis, "document", originalDocument)
    else Reflect.deleteProperty(globalThis, "document")
    if (originalNavigator) Object.defineProperty(globalThis, "navigator", originalNavigator)
    else Reflect.deleteProperty(globalThis, "navigator")
    globals.IS_REACT_ACT_ENVIRONMENT = originalActEnvironment
    testWindow.close()
  }
})
