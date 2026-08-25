// Copies public/assets/SnapDrop_Square.png to scripts/app-icon.png for Tauri icon generation.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const srcSquare = path.join(__dirname, "..", "public", "assets", "SnapDrop_Square.png");
const destIcon = path.join(__dirname, "app-icon.png");

if (fs.existsSync(srcSquare)) {
  fs.copyFileSync(srcSquare, destIcon);
  console.log(`Copied ${srcSquare} -> ${destIcon}`);
  
  // Also copy to public/favicon.png
  const faviconPath = path.join(__dirname, "..", "public", "favicon.png");
  fs.copyFileSync(srcSquare, faviconPath);
  console.log(`Copied ${srcSquare} -> ${faviconPath}`);
} else {
  console.error(`Source square icon not found at ${srcSquare}`);
}
