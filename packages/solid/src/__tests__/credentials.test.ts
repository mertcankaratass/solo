import { beforeEach, describe, expect, it, vi } from "vitest"

const platform = vi.hoisted(() => ({
  writeCredentials: vi.fn(),
  readCredentials: vi.fn(),
  deleteCredentials: vi.fn(),
}))

vi.mock("@solo/native", async (original) => ({
  ...await original<typeof import("@solo/native")>(),
  ...platform,
}))

let credentialsModule: typeof import("../credentials.js")

beforeEach(async () => {
  vi.resetModules()
  vi.resetAllMocks()
  credentialsModule = await import("../credentials.js")
})

describe("credentials.write", () => {
  it("passes arguments to native with resolved appId", async () => {
    platform.writeCredentials.mockResolvedValueOnce(undefined)

    await credentialsModule.credentials.write(
      "https://github.com",
      "octacat",
      "secret-token-123",
      "dev.solo.test",
    )

    expect(platform.writeCredentials).toHaveBeenCalledExactlyOnceWith(
      "https://github.com",
      "octacat",
      "secret-token-123",
      "dev.solo.test",
    )
  })

  it("accepts Uint8Array as secret and converts to UTF-8 string", async () => {
    platform.writeCredentials.mockResolvedValueOnce(undefined)
    const secretBytes = new TextEncoder().encode("binary-secret-token")

    await credentialsModule.credentials.write(
      "https://api.example.com",
      "bot",
      secretBytes,
      "dev.solo.test",
    )

    expect(platform.writeCredentials).toHaveBeenCalledWith(
      "https://api.example.com",
      "bot",
      "binary-secret-token",
      "dev.solo.test",
    )
  })

  it("rejects Uint8Array containing invalid UTF-8 data", async () => {
    const invalidBytes = new Uint8Array([0xff, 0xff])

    await expect(
      credentialsModule.credentials.write(
        "https://api.example.com",
        "bot",
        invalidBytes,
        "dev.solo.test",
      ),
    ).rejects.toThrow(TypeError)

    expect(platform.writeCredentials).not.toHaveBeenCalled()
  })

  it("throws when neither solo.identifier nor appId is present", async () => {
    await expect(
      credentialsModule.credentials.write(
        "https://api.example.com",
        "bot",
        "secret",
      ),
    ).rejects.toThrow("Credential operations need the app identifier")
  })

  it("rejects invalid arguments before dispatch", async () => {
    await expect(
      credentialsModule.credentials.write("", "user", "secret"),
    ).rejects.toThrow(TypeError)

    await expect(
      credentialsModule.credentials.write("https://api.example.com", "", "secret"),
    ).rejects.toThrow(TypeError)

    await expect(
      credentialsModule.credentials.write("https://api.example.com", "user", ""),
    ).rejects.toThrow(TypeError)

    await expect(
      credentialsModule.credentials.write(
        "https://api.example.com",
        "user",
        new Uint8Array(0),
      ),
    ).rejects.toThrow(TypeError)

    await expect(
      credentialsModule.credentials.write(
        "https://api.example.com",
        "user",
        123 as never,
      ),
    ).rejects.toThrow(TypeError)

    expect(platform.writeCredentials).not.toHaveBeenCalled()
  })

  it("surfaces native errors without leaking secrets", async () => {
    const error = new Error("No Secret Service provider available on the session bus")
    platform.writeCredentials.mockRejectedValueOnce(error)

    await expect(
      credentialsModule.credentials.write(
        "https://api.example.com",
        "user",
        "super-secret-pw",
      ),
    ).rejects.toThrow("No Secret Service provider available on the session bus")
  })
})

describe("credentials.read", () => {
  it("returns { username, secret } when credential exists", async () => {
    platform.readCredentials.mockResolvedValueOnce({
      username: "octacat",
      secret: "token-abc-xyz",
    })

    const entry = await credentialsModule.credentials.read(
      "https://github.com",
      "dev.solo.test",
    )

    expect(entry).toEqual({
      username: "octacat",
      secret: "token-abc-xyz",
    })
    expect(platform.readCredentials).toHaveBeenCalledExactlyOnceWith(
      "https://github.com",
      "dev.solo.test",
    )
  })

  it("returns null when no credential matches", async () => {
    platform.readCredentials.mockResolvedValueOnce(null)

    const entry = await credentialsModule.credentials.read("https://missing.com", "dev.solo.test")

    expect(entry).toBeNull()
    expect(platform.readCredentials).toHaveBeenCalledWith(
      "https://missing.com",
      "dev.solo.test",
    )
  })

  it("rejects invalid URL", async () => {
    await expect(credentialsModule.credentials.read("", "dev.solo.test")).rejects.toThrow(TypeError)
    await expect(credentialsModule.credentials.read(null as never, "dev.solo.test")).rejects.toThrow(TypeError)
    expect(platform.readCredentials).not.toHaveBeenCalled()
  })
})

describe("credentials.delete", () => {
  it("returns true when entry was deleted", async () => {
    platform.deleteCredentials.mockResolvedValueOnce(true)

    const result = await credentialsModule.credentials.delete(
      "https://github.com",
      "dev.solo.test",
    )

    expect(result).toBe(true)
    expect(platform.deleteCredentials).toHaveBeenCalledExactlyOnceWith(
      "https://github.com",
      "dev.solo.test",
    )
  })

  it("returns false when no entry was found", async () => {
    platform.deleteCredentials.mockResolvedValueOnce(false)

    const result = await credentialsModule.credentials.delete("https://missing.com", "dev.solo.test")

    expect(result).toBe(false)
    expect(platform.deleteCredentials).toHaveBeenCalledWith(
      "https://missing.com",
      "dev.solo.test",
    )
  })

  it("rejects invalid URL", async () => {
    await expect(credentialsModule.credentials.delete("", "dev.solo.test")).rejects.toThrow(TypeError)
    expect(platform.deleteCredentials).not.toHaveBeenCalled()
  })
})

describe("top-level exports", () => {
  it("exports writeCredential, readCredential, deleteCredential matching credentials object", () => {
    expect(credentialsModule.writeCredential).toBe(credentialsModule.credentials.write)
    expect(credentialsModule.readCredential).toBe(credentialsModule.credentials.read)
    expect(credentialsModule.deleteCredential).toBe(credentialsModule.credentials.delete)
  })
})
