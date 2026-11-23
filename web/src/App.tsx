import { useCallback, useEffect, useRef, useState } from 'react';
import { HardDrive } from 'lucide-react';
import { AuthGate } from './components/AuthGate';
import { StatusBanner } from './components/StatusBanner';
import { Workspace } from './components/Workspace';
import { ApiClientError, getStoredToken, probeHealth, setStoredToken } from './lib/api';
import type { ConnectionState, HealthResponse } from './types/api';

type Session =
  | { kind: 'checking' }
  | { kind: 'login'; error: string | null }
  | { kind: 'ready'; health: HealthResponse; token: string | null };

function connectionFailure(): Exclude<ConnectionState, 'connected'> {
  return navigator.onLine ? 'disconnected' : 'offline';
}

export function App() {
  const [theme, setTheme] = useState<'light' | 'dark'>(() =>
    document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light',
  );
  const [session, setSession] = useState<Session>({ kind: 'checking' });
  const [connection, setConnection] = useState<ConnectionState>('connected');
  const [busy, setBusy] = useState(false);
  const activeProbe = useRef<AbortController | null>(null);

  const toggleTheme = () => {
    const next = theme === 'light' ? 'dark' : 'light';
    document.documentElement.dataset.theme = next;
    setTheme(next);
    try {
      localStorage.setItem('taildrive_theme', next);
    } catch {
      // The theme still applies for this page.
    }
  };

  const connect = useCallback(async (enteredToken?: string) => {
    activeProbe.current?.abort();
    const controller = new AbortController();
    activeProbe.current = controller;
    setBusy(true);
    const signal = AbortSignal.any([controller.signal, AbortSignal.timeout(5000)]);
    let token: string | null = enteredToken ?? null;
    try {
      let health: HealthResponse;
      try {
        health = await probeHealth(token, signal);
      } catch (error) {
        if (!(error instanceof ApiClientError) || error.status !== 401 || enteredToken) throw error;
        token = getStoredToken();
        if (!token) throw error;
        health = await probeHealth(token, signal);
      }
      if (controller.signal.aborted) return;
      setStoredToken(token);
      setSession({ kind: 'ready', health, token });
      setConnection('connected');
    } catch (error) {
      if (controller.signal.aborted) return;
      if (error instanceof ApiClientError && error.status === 401) {
        setStoredToken(null);
        setConnection('connected');
        setSession({ kind: 'login', error: token ? 'TailDrive rejected this Bearer token.' : null });
      } else {
        setConnection(connectionFailure());
        if (enteredToken) setSession({ kind: 'login', error: 'Unable to reach TailDrive.' });
      }
    } finally {
      if (activeProbe.current === controller) {
        activeProbe.current = null;
        setBusy(false);
      }
    }
  }, []);

  useEffect(() => {
    void connect();
    const offline = () => {
      activeProbe.current?.abort();
      setConnection('offline');
    };
    const online = () => { void connect(); };
    window.addEventListener('offline', offline);
    window.addEventListener('online', online);
    return () => {
      activeProbe.current?.abort();
      window.removeEventListener('offline', offline);
      window.removeEventListener('online', online);
    };
  }, [connect]);

  useEffect(() => {
    if (connection === 'connected') return;
    const timer = window.setInterval(() => {
      if (navigator.onLine && !activeProbe.current) void connect();
    }, 10_000);
    return () => window.clearInterval(timer);
  }, [connect, connection]);

  const reportApiError = useCallback((error: unknown) => {
    if (!(error instanceof ApiClientError)) return;
    if (error.isNetworkError) {
      setConnection(connectionFailure());
    } else if (error.status === 401 && !activeProbe.current) {
      void connect();
    }
  }, [connect]);

  if (session.kind === 'ready') {
    return (
      <Workspace
        key={session.token ? 'bearer' : session.health.tailnet_user}
        health={session.health}
        hasToken={session.token !== null}
        connection={connection}
        probing={busy}
        onReconnect={() => { void connect(); }}
        onApiError={reportApiError}
        onLogout={() => {
          setStoredToken(null);
          setSession({ kind: 'checking' });
          void connect();
        }}
        onQuota={(quota) => {
          setSession((current) => current.kind === 'ready'
            ? { ...current, health: { ...current.health, quota } }
            : current);
        }}
        theme={theme}
        onToggleTheme={toggleTheme}
      />
    );
  }

  if (session.kind === 'login') {
    return (
      <AuthGate
        theme={theme}
        onToggleTheme={toggleTheme}
        onAuthenticate={connect}
        errorMessage={session.error}
        busy={busy}
      />
    );
  }

  return (
    <>
      <StatusBanner connection={connection} nodeName="macmini-ci" retrying={busy} onRetry={() => { void connect(); }} />
      <div className="auth-screen">
        <div className="brand" role="status">
          <HardDrive size={16} />
          <span>{connection === 'connected' ? 'Connecting to TailDrive…' : 'Waiting for TailDrive'}</span>
        </div>
      </div>
    </>
  );
}
