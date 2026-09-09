// Development-only entrypoint. `npm run build:graph` turns Mermaid and ELK into
// one checked-in local ESM file; the Rust runtime serves that file directly.
import mermaid from 'mermaid';
import elkLayouts from '@mermaid-js/layout-elk';

mermaid.registerLayoutLoaders(elkLayouts);

export default mermaid;
