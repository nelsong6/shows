import {defineConfig} from 'vite'
import react from '@vitejs/plugin-react'

// https://vitejs.dev/config/
export default defineConfig({
  // Relative asset URLs so the bundle can be served from the desktop-qt
  // control server's root (it serves index.html + /assets directly).
  base: './',
  plugins: [react()]
})
