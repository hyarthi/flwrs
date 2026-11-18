// The management API: JSON in and out, errors as `ApiError` (the server's
// `{error, details[]}`). The session cookie goes along on its own (same
// origin). A 401 outside `/auth/` means the session ended: `onUnauthorized`
// sends the user to the login page.

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly details: string[] = [],
  ) {
    super(message);
  }
}

let onUnauthorized: (() => void) | null = null;

/** Called on a 401 from a protected endpoint (set by the session state). */
export function setUnauthorizedHandler(handler: () => void) {
  onUnauthorized = handler;
}

export async function api<T>(path: string, init: RequestInit & { json?: unknown } = {}): Promise<T> {
  const { json, headers, ...rest } = init;
  const request: RequestInit = {
    credentials: 'same-origin',
    ...rest,
    headers: {
      Accept: 'application/json',
      ...(json !== undefined ? { 'Content-Type': 'application/json' } : {}),
      ...headers,
    },
    body: json !== undefined ? JSON.stringify(json) : rest.body,
  };

  let response: Response;
  try {
    response = await fetch(`/api${path}`, request);
  } catch {
    throw new ApiError(0, 'The server is unreachable');
  }

  const text = await response.text();
  let body: unknown = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = null;
    }
  }

  if (!response.ok) {
    if (response.status === 401 && !path.startsWith('/auth/')) onUnauthorized?.();
    const { error, details } = (body ?? {}) as { error?: string; details?: string[] };
    throw new ApiError(response.status, error ?? `${response.status} ${response.statusText}`, details ?? []);
  }
  return body as T;
}
