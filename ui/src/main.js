import { mount } from 'svelte';
import './tokens.css';
import App from './App.svelte';

// Svelte 5 用 mount() 而非 new App()
export default mount(App, { target: document.getElementById('app') });
