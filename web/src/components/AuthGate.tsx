import { useEffect, useRef, useState, type FormEvent } from 'react';
import { HardDrive, KeyRound, Moon, Sun } from 'lucide-react';

interface AuthGateProps {
  theme: 'light' | 'dark';
  onToggleTheme: () => void;
  onAuthenticate: (token: string) => Promise<void>;
  errorMessage: string | null;
  busy: boolean;
}

export function AuthGate({
  theme,
  onToggleTheme,
  onAuthenticate,
  errorMessage,
  busy,
}: AuthGateProps) {
  const [token, setToken] = useState('');
  const [localError, setLocalError] = useState<string | null>(null);
  const ref = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    ref.current?.showModal();
  }, []);

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = token.trim();
    if (!trimmed) {
      setLocalError('Enter a valid Bearer token to continue.');
      return;
    }
    setLocalError(null);
    await onAuthenticate(trimmed);
  };

  const displayError = localError || errorMessage;

  return (
    <div className="auth-screen">
      <dialog ref={ref} className="auth-card workspace-dialog" aria-labelledby="auth-title" onCancel={(event) => event.preventDefault()}>
        <div className="auth-head">
          <div className="brand">
            <span className="brand-mark">
              <HardDrive size={13} strokeWidth={2} />
            </span>
            <span id="auth-title">Sign in to TailDrive</span>
          </div>
          <button
            type="button"
            className="icon-btn"
            onClick={onToggleTheme}
            title={theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}
            aria-label="Toggle color theme"
          >
            {theme === 'dark' ? <Sun size={15} /> : <Moon size={15} />}
          </button>
        </div>

        <p className="auth-desc">
          Enter a Bearer token to access your drive on this connection.
        </p>

        <form onSubmit={handleSubmit} style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
          <div className="field">
            <label htmlFor="taildrive-token" className="field-label">
              Bearer Token
            </label>
            <input
              id="taildrive-token"
              type="password"
              className="text-input mono"
              placeholder="td_live_••••••••••••••••"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              autoFocus
              autoComplete="current-password"
              disabled={busy}
            />
          </div>

          {displayError && (
            <div className="form-error" role="alert">
              {displayError}
            </div>
          )}

          <button type="submit" className="btn btn-primary" disabled={busy} style={{ height: 36 }}>
            <KeyRound size={14} />
            <span>{busy ? 'Verifying…' : 'Sign In'}</span>
          </button>
        </form>

      </dialog>
    </div>
  );
}
