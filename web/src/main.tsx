import { createRoot } from 'react-dom/client';
import { App } from './App';
import './styles/tokens.css';
import './styles/app.css';

const root = document.getElementById('root');
if (!root) throw new Error('Missing TailDrive root element');
createRoot(root).render(<App />);
