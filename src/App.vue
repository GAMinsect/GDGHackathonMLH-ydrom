<script setup>
import { ref, reactive, computed, onMounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { GoogleGenAI } from "@google/genai";

// Application State
const isApiReady = ref(false);
const apiKeyError = ref("");
const isScanning = ref(false);
const scanStatus = ref("");
const imageUrl = ref("");
const imageFile = ref(null);
const selectedBoxId = ref(null);

// AI Instance
let ai = null;

// Scan Results
const detectedContext = ref("");
const documentSummary = ref("");
const boxes = ref([]);
const boxStates = reactive({});

// Context-aware Gemma 4 / Gemini Prompt
const SCANNER_PROMPT = `
You are an expert legal, contract, and privacy vision analyst.
Analyze this contract or document image.

Step 1: Identify the DOCUMENT CONTEXT (e.g. "Residential Apartment Lease", "Employment Agreement", "Freelance / NDA Agreement", "Consumer / Gym Subscription").
Step 2: Based on that specific context, identify all CRITICAL CONTENTS that must be flagged:
  - Personal Data (PII): Full names of tenants/employees, physical residential addresses, personal phone numbers, emails, bank IBAN / account numbers.
  - Contextual Overreach & Counterpart Control Traps:
    * In employment: non-compete clauses, bans on working for competitors, excessive notice periods, claiming ownership over personal side-projects.
    * In leases: unilateral rent or fee increase rights, abusive deposit withholding, excessive notice periods (e.g. 6 months).
    * In subscriptions/services: automatic renewals, penalty termination fees.
  - Financial Commitments: Exact monthly rent/salary numbers, security deposit sums.

Step 3: Return the exact spatial bounding boxes for each critical content item using normalized coordinates [ymin, xmin, ymax, xmax] where 0 is top/left and 1000 is bottom/right.

You must respond ONLY with a valid JSON object matching this schema:
{
  "detected_context": "Short title of document type",
  "summary": "1-2 sentence plain-language summary of what this document is and its risk level",
  "boxes": [
    {
      "id": "box_1",
      "box_2d": [ymin, xmin, ymax, xmax],
      "label": "Short label (e.g., Bank IBAN)",
      "category": "PII" | "OVERREACH" | "FINANCIAL",
      "severity": "CRITICAL" | "HIGH" | "MEDIUM",
      "reason": "Plain-language context-dependent reason explaining why this area is critical",
      "default_obscure": true
    }
  ]
}
`;

// Gemma 4 models available through the Gemini API (open-weight, vision-capable)
const GEMMA_MODELS = ["gemma-4-26b-a4b-it", "gemma-4-31b-it"];

// Extract the JSON object from the model output (handles ``` fences or stray text)
function parseModelJson(text) {
  const raw = (text || "").trim();
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start === -1 || end <= start) {
    throw new Error("Risposta del modello non valida (JSON mancante).");
  }
  return JSON.parse(raw.slice(start, end + 1));
}

// Keep only well-formed boxes and fill missing fields so the UI never breaks
function normalizeBoxes(rawBoxes) {
  const CATEGORIES = ["PII", "OVERREACH", "FINANCIAL"];
  return (Array.isArray(rawBoxes) ? rawBoxes : [])
    .filter(
      (b) =>
        Array.isArray(b?.box_2d) &&
        b.box_2d.length === 4 &&
        b.box_2d.every((n) => Number.isFinite(Number(n)))
    )
    .map((b, i) => {
      const [ymin, xmin, ymax, xmax] = b.box_2d.map(Number);
      const category = String(b.category || "").toUpperCase();
      return {
        id: `box_${i + 1}`,
        box_2d: [
          Math.min(ymin, ymax),
          Math.min(xmin, xmax),
          Math.max(ymin, ymax),
          Math.max(xmin, xmax),
        ],
        label: b.label || "Contenuto critico",
        category: CATEGORIES.includes(category) ? category : "PII",
        severity: b.severity || "HIGH",
        reason: b.reason || "",
        default_obscure: b.default_obscure ?? true,
      };
    });
}

// Initialize API Key from Tauri Backend
onMounted(async () => {
  try {
    const apiKeys = await invoke("my_get_api_keys", {});
    if (!apiKeys || !apiKeys.GEMINI_API) {
      apiKeyError.value = "Chiave GEMINI_API non trovata nel file .env.";
      return;
    }
    ai = new GoogleGenAI({ apiKey: apiKeys.GEMINI_API });
    isApiReady.value = true;
    console.log("Gemma / Gemini Vision API inizializzata con successo!");
  } catch (err) {
    console.error("Errore nel recupero delle chiavi:", err);
    apiKeyError.value = "Impossibile recuperare le chiavi API dal backend Tauri.";
  }
});

// File Ingestion Handler
function onFileSelected(event) {
  const file = event.target.files?.[0];
  if (!file) return;
  loadImageFile(file);
}

function onFileDrop(event) {
  event.preventDefault();
  const file = event.dataTransfer.files?.[0];
  if (!file) return;
  loadImageFile(file);
}

function loadImageFile(file) {
  imageFile.value = file;
  const reader = new FileReader();
  reader.onload = (e) => {
    imageUrl.value = e.target.result;
    // Auto-scan on image load
    scanDocumentImage();
  };
  reader.readAsDataURL(file);
}

// Convert Base64 and Call Vision AI
async function scanDocumentImage() {
  if (!ai || !imageUrl.value) return;

  isScanning.value = true;
  scanStatus.value = "Gemma sta analizzando il contesto del documento...";
  boxes.value = [];

  try {
    const rawBase64 = imageUrl.value.split(",")[1];
    const mimeType = imageFile.value?.type || "image/jpeg";

    scanStatus.value = "Rilevamento aree critiche e dati sensibili...";

    // Gemma 4 (open-weight, multimodal) is the inner engine.
    // Try the faster MoE variant first, then the larger dense one.
    let response;
    let lastError;
    for (const model of GEMMA_MODELS) {
      try {
        response = await ai.models.generateContent({
          model,
          contents: [
            {
              inlineData: {
                data: rawBase64,
                mimeType: mimeType,
              },
            },
            { text: SCANNER_PROMPT },
          ],
          config: {
            responseMimeType: "application/json",
          },
        });
        console.log(`Scansione completata con ${model}`);
        break;
      } catch (err) {
        console.warn(`Modello ${model} non disponibile:`, err);
        lastError = err;
      }
    }
    if (!response) throw lastError;

    const result = parseModelJson(response.text);

    detectedContext.value = result.detected_context || "Documento Contrattuale";
    documentSummary.value = result.summary || "Analisi completata con successo.";
    boxes.value = normalizeBoxes(result.boxes);

    // Initialize interactive state for each box
    boxes.value.forEach((b) => {
      boxStates[b.id] = {
        obscured: b.default_obscure ?? true,
        hallucination: false,
      };
    });
  } catch (error) {
    console.error("Errore durante la scansione:", error);
    alert("Errore durante la scansione con l'IA: " + error.message);
  } finally {
    isScanning.value = false;
    scanStatus.value = "";
  }
}

// Convert coordinates (0-1000) to CSS percentage
function getBoxStyle(box) {
  const [ymin, xmin, ymax, xmax] = box.box_2d;
  // Handle both 0-1000 normalized and 0-1 floats
  const factor = ymax <= 1.0 ? 100 : 0.1;

  const top = ymin * factor;
  const left = xmin * factor;
  const height = (ymax - ymin) * factor;
  const width = (xmax - xmin) * factor;

  return {
    top: `${top}%`,
    left: `${left}%`,
    height: `${height}%`,
    width: `${width}%`,
  };
}

// Interactive Box Actions
function toggleObscure(boxId) {
  if (boxStates[boxId]) {
    boxStates[boxId].obscured = !boxStates[boxId].obscured;
  }
}

function flagHallucination(boxId) {
  if (boxStates[boxId]) {
    boxStates[boxId].hallucination = true;
    boxStates[boxId].obscured = false;
  }
}

function obscureAll() {
  boxes.value.forEach((b) => {
    if (boxStates[b.id] && !boxStates[b.id].hallucination) {
      boxStates[b.id].obscured = true;
    }
  });
}

function revealAll() {
  boxes.value.forEach((b) => {
    if (boxStates[b.id]) {
      boxStates[b.id].obscured = false;
    }
  });
}

function resetAll() {
  imageUrl.value = "";
  imageFile.value = null;
  boxes.value = [];
  detectedContext.value = "";
  documentSummary.value = "";
}

// Load Mock Demo for instant testing without camera
function loadDemoSample() {
  detectedContext.value = "Contratto di Locazione Abitativa (Demo)";
  documentSummary.value =
    "Rilevate 4 aree critiche: dati personali del conduttore, coordinate bancarie IBAN e clausola vessatoria di recesso bloccato a 6 mesi.";

  // Create a clean dummy canvas placeholder
  const canvas = document.createElement("canvas");
  canvas.width = 800;
  canvas.height = 1100;
  const ctx = canvas.getContext("2d");

  // Background page
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(0, 0, 800, 1100);

  // Decorative text lines simulating a contract
  ctx.fillStyle = "#1e293b";
  ctx.font = "bold 26px sans-serif";
  ctx.fillText("CONTRATTO DI LOCAZIONE AD USO ABITATIVO", 60, 80);

  ctx.font = "16px sans-serif";
  ctx.fillStyle = "#475569";
  ctx.fillText("Tra il Locatore Immobiliare SpA e il Conduttore Sig. Mario Rossi,", 60, 140);
  ctx.fillText("residente in Via Garibaldi 12, Milano (MI) - C.F. RSSMRA95M01H501Z.", 60, 170);

  ctx.fillText("Art. 1 - Canone di Locazione: L'importo concordato è pari a € 750,00/mese.", 60, 260);
  ctx.fillText("Art. 2 - Il pagamento avverrà tramite bonifico bancario su IBAN:", 60, 300);
  ctx.font = "bold 16px sans-serif";
  ctx.fillText("IT60X0542811101000000123456 - Intestato a Gestione Immobili Srl.", 60, 330);

  ctx.font = "16px sans-serif";
  ctx.fillText("Art. 3 - Clausola Vessatoria di Recesso e Aumenti Canone:", 60, 480);
  ctx.fillStyle = "#dc2626";
  ctx.fillText("Il conduttore NON potrà recedere prima di 24 mesi e con preavviso", 60, 520);
  ctx.fillText("obbligatorio di 6 mesi. Il locatore si riserva il diritto unilaterale", 60, 550);
  ctx.fillText("di aumentare il canone del 15% in caso di incremento costi generali.", 60, 580);

  imageUrl.value = canvas.toDataURL("image/png");

  boxes.value = [
    {
      id: "demo_1",
      box_2d: [115, 60, 150, 450],
      label: "Nome Conduttore: Mario Rossi",
      category: "PII",
      severity: "CRITICAL",
      reason: "Identifica direttamente la persona che firma il contratto.",
      default_obscure: true,
    },
    {
      id: "demo_2",
      box_2d: [155, 60, 185, 520],
      label: "Indirizzo Residenza & Codice Fiscale",
      category: "PII",
      severity: "CRITICAL",
      reason: "Dati anagrafici e di residenza ad alto rischio di furto d'identità.",
      default_obscure: true,
    },
    {
      id: "demo_3",
      box_2d: [315, 60, 345, 500],
      label: "Coordinate Bancarie IBAN",
      category: "FINANCIAL",
      severity: "CRITICAL",
      reason: "Conto corrente bancario esposto nel documento.",
      default_obscure: true,
    },
    {
      id: "demo_4",
      box_2d: [500, 50, 600, 720],
      label: "Overreach: Recesso Bloccato & Aumento Unilaterale",
      category: "OVERREACH",
      severity: "CRITICAL",
      reason:
        "Clausola fortemente sbilanciata a favore della controparte con divieto di recesso e aumento arbitrario.",
      default_obscure: true,
    },
  ];

  boxes.value.forEach((b) => {
    boxStates[b.id] = { obscured: true, hallucination: false };
  });
}

// Active boxes (excluding hallucinations)
const activeBoxes = computed(() => {
  return boxes.value.filter((b) => !boxStates[b.id]?.hallucination);
});

const criticalCount = computed(() => {
  return activeBoxes.value.filter((b) => b.severity === "CRITICAL").length;
});

async function saveCensoredDocument() {
	if (!imageUrl.value) return;

	try {
		// 1. Carica l'immagine originale in memoria per ottenere le dimensioni reali
		const img = new Image();
		img.src = imageUrl.value;
		await new Promise((resolve, reject) => {
			img.onload = resolve;
			img.onerror = reject;
		});

		// 2. Crea un canvas con la risoluzione nativa dell'immagine
		const canvas = document.createElement("canvas");
		canvas.width = img.naturalWidth || img.width;
		canvas.height = img.naturalHeight || img.height;
		const ctx = canvas.getContext("2d");

		// 3. Disegna l'immagine di base sul canvas
		ctx.drawImage(img, 0, 0);

		// 4. Disegna i rettangoli neri definitivi sopra tutte le aree oscurate
		ctx.fillStyle = "#000000";
		activeBoxes.value.forEach((box) => {
			if (boxStates[box.id]?.obscured) {
				const [ymin, xmin, ymax, xmax] = box.box_2d;
				// Converte le coordinate (0-1000 o 0-1) in frazione (0.0 - 1.0)
				const factor = ymax <= 1.0 ? 1.0 : 0.001;

				const x = xmin * factor * canvas.width;
				const y = ymin * factor * canvas.height;
				const w = (xmax - xmin) * factor * canvas.width;
				const h = (ymax - ymin) * factor * canvas.height;

				ctx.fillRect(x, y, w, h);
			}
		});

		// 5. Esporta il canvas in Base64 PNG e invoca il popup di salvataggio in Rust
		const censoredDataUrl = canvas.toDataURL("image/png");
		const savedPath = await invoke("save_censored_image", {
			base64Image: censoredDataUrl,
		});

		if (savedPath) {
			alert(`Immagine censurata salvata con successo in:\n${savedPath}`);
		}
	} catch (error) {
		console.error("Errore durante il salvataggio:", error);
		alert("Errore durante il salvataggio dell'immagine: " + error);
	}
}
</script>

<template>
  <main class="app-shell">
    <!-- Header -->
    <header class="app-header">
      <div class="brand">
        <span class="brand-badge">PROTOTIPO</span>
        <h1>Scanner Clausole & Privacy</h1>
      </div>
      <div class="api-status">
        <span v-if="isApiReady" class="status-pill ready">● Gemma 4 Pronta</span>
        <span v-else-if="apiKeyError" class="status-pill error">⚠ {{ apiKeyError }}</span>
        <span v-else class="status-pill loading">○ Inizializzazione...</span>
      </div>
    </header>

    <!-- Upload Screen (When no image is loaded) -->
    <section v-if="!imageUrl" class="upload-container">
      <div
        class="dropzone"
        @dragover.prevent
        @drop="onFileDrop"
      >
        <div class="dropzone-icon">📄</div>
        <h2>Carica o Trascina una Foto del Contratto</h2>
        <p>Supporta foto da smartphone, screenshot e scansioni (PNG, JPG, WEBP)</p>

        <div class="upload-actions">
          <label class="btn-primary">
            📁 Scegli Immagine
            <input
              type="file"
              accept="image/*"
              style="display: none"
              @change="onFileSelected"
            />
          </label>
          <button class="btn-secondary" @click="loadDemoSample">
            ⚡ Carica Contratto Demo
          </button>
        </div>
      </div>
    </section>

    <!-- Inspection Screen (When image is loaded) -->
    <section v-else class="inspection-layout">
      <!-- Left Column: Interactive Image Viewer with Overlaid Bounding Boxes -->
      <div class="viewer-card">
        <!-- Control Toolbar -->
        <div class="viewer-toolbar">
          <div class="context-info">
            <span class="context-tag">{{ detectedContext || "Analisi in corso..." }}</span>
            <span v-if="activeBoxes.length > 0" class="flags-count">
              🚨 {{ activeBoxes.length }} aree critiche rilevate ({{ criticalCount }} ad alto rischio)
            </span>
          </div>

			<div class="toolbar-buttons">
				<button class="btn-action" @click="obscureAll" title="Nascondi tutte le aree suggerite">
					🛡️ Oscura Tutto
				</button>
				<button class="btn-action outline" @click="revealAll" title="Mostra tutte le aree">
					👁️ Mostra Tutto
				</button>
				<button class="btn-action save" @click="saveCensoredDocument" title="Esporta e salva l'immagine censurata">
					💾 Salva Censurato
				</button>
				<button class="btn-action danger" @click="resetAll">
					✕ Nuovo Documento
				</button>
			</div>
        </div>

        <!-- Scanning Loader Overlay -->
        <div v-if="isScanning" class="scanning-overlay">
          <div class="scanner-laser"></div>
          <div class="scanner-card">
            <div class="spinner"></div>
            <p>{{ scanStatus }}</p>
          </div>
        </div>

        <!-- Image Container with Absolute Bounding Box Overlay -->
        <div class="image-stage">
          <div class="image-wrapper">
            <img :src="imageUrl" alt="Documento Contrattuale" class="document-image" />

            <!-- Dynamic Bounding Boxes -->
            <div
              v-for="box in activeBoxes"
              :key="box.id"
              :style="getBoxStyle(box)"
              :class="[
                'bounding-box',
                box.category.toLowerCase(),
                {
                  'is-obscured': boxStates[box.id]?.obscured,
                  'is-selected': selectedBoxId === box.id,
                },
              ]"
              @mouseenter="selectedBoxId = box.id"
              @mouseleave="selectedBoxId = null"
              @click="toggleObscure(box.id)"
            >
              <!-- Obscured Blackout Mask -->
              <div v-if="boxStates[box.id]?.obscured" class="blackout-mask">
                <span class="mask-label">OSCURATO</span>
              </div>

              <!-- Interactive Hover Tooltip -->
              <div class="box-tooltip" @click.stop>
                <div class="tooltip-header">
                  <span :class="['category-badge', box.category.toLowerCase()]">
                    {{ box.category }}
                  </span>
                  <span class="severity-badge">{{ box.severity }}</span>
                </div>
                <div class="tooltip-title">{{ box.label }}</div>
                <div class="tooltip-reason">{{ box.reason }}</div>

                <div class="tooltip-actions">
                  <button
                    class="btn-tooltip-toggle"
                    @click="toggleObscure(box.id)"
                  >
                    {{ boxStates[box.id]?.obscured ? "👁️ Mostra Testo" : "🛡️ Oscura" }}
                  </button>
                  <button
                    class="btn-tooltip-flag"
                    @click="flagHallucination(box.id)"
                    title="Segnala come allucinazione dell'IA"
                  >
                    ⚠️ Errore IA
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>

      <!-- Right Column: Critical Findings & Context Inspector -->
      <aside class="inspector-sidebar">
        <div class="inspector-header">
          <h3>Rilevamenti Critici nel Contesto</h3>
          <p class="summary-text">{{ documentSummary }}</p>
        </div>

        <div class="box-list">
          <div
            v-for="box in activeBoxes"
            :key="'list_' + box.id"
            :class="[
              'box-item',
              box.category.toLowerCase(),
              {
                'active': selectedBoxId === box.id,
                'obscured': boxStates[box.id]?.obscured,
              },
            ]"
            @mouseenter="selectedBoxId = box.id"
            @mouseleave="selectedBoxId = null"
          >
            <div class="box-item-header">
              <span class="box-item-title">{{ box.label }}</span>
              <span :class="['category-badge', box.category.toLowerCase()]">
                {{ box.category }}
              </span>
            </div>
            <p class="box-item-reason">{{ box.reason }}</p>

            <div class="box-item-footer">
              <label class="switch-control">
                <input
                  type="checkbox"
                  :checked="boxStates[box.id]?.obscured"
                  @change="toggleObscure(box.id)"
                />
                <span>{{ boxStates[box.id]?.obscured ? "Oscurato" : "Visibile" }}</span>
              </label>

              <button
                class="btn-discard"
                @click="flagHallucination(box.id)"
                title="Segnala come falso positivo"
              >
                Ignora (Errore IA)
              </button>
            </div>
          </div>
        </div>
      </aside>
    </section>
  </main>
</template>

<style scoped>
/* App Shell & Header */
.app-shell {
  min-height: 100vh;
  display: flex;
  flex-direction: column;
  background-color: #0f172a;
  color: #f8fafc;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}

.app-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 1rem 2rem;
  background-color: #1e293b;
  border-bottom: 1px solid #334155;
}

.brand {
  display: flex;
  align-items: center;
  gap: 0.75rem;
}

.brand h1 {
  font-size: 1.25rem;
  font-weight: 700;
  margin: 0;
  color: #ffffff;
}

.brand-badge {
  background-color: #6366f1;
  color: #ffffff;
  font-size: 0.65rem;
  font-weight: 800;
  padding: 0.2rem 0.5rem;
  border-radius: 9999px;
  letter-spacing: 0.05em;
}

.status-pill {
  font-size: 0.8rem;
  padding: 0.35rem 0.75rem;
  border-radius: 9999px;
  font-weight: 600;
}

.status-pill.ready {
  background-color: #064e3b;
  color: #34d399;
}

.status-pill.error {
  background-color: #7f1d1d;
  color: #fca5a5;
}

.status-pill.loading {
  background-color: #334155;
  color: #94a3b8;
}

/* Dropzone Screen */
.upload-container {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 2rem;
}

.dropzone {
  max-width: 600px;
  width: 100%;
  padding: 3.5rem 2rem;
  background-color: #1e293b;
  border: 2px dashed #475569;
  border-radius: 1.25rem;
  text-align: center;
  transition: border-color 0.2s;
}

.dropzone:hover {
  border-color: #6366f1;
}

.dropzone-icon {
  font-size: 3.5rem;
  margin-bottom: 1rem;
}

.dropzone h2 {
  font-size: 1.4rem;
  font-weight: 700;
  margin-bottom: 0.5rem;
}

.dropzone p {
  color: #94a3b8;
  font-size: 0.9rem;
  margin-bottom: 2rem;
}

.upload-actions {
  display: flex;
  justify-content: center;
  gap: 1rem;
}

.btn-primary {
  background-color: #6366f1;
  color: white;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 600;
  cursor: pointer;
  border: none;
  transition: background-color 0.2s;
}

.btn-primary:hover {
  background-color: #4f46e5;
}

.btn-secondary {
  background-color: #334155;
  color: #e2e8f0;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 600;
  cursor: pointer;
  border: 1px solid #475569;
  transition: background-color 0.2s;
}

.btn-secondary:hover {
  background-color: #475569;
}

/* Inspection Layout */
.inspection-layout {
  flex: 1;
  display: grid;
  grid-template-columns: 1fr 380px;
  gap: 1.5rem;
  padding: 1.5rem;
  overflow: hidden;
}

.viewer-card {
  display: flex;
  flex-direction: column;
  background-color: #1e293b;
  border-radius: 1rem;
  border: 1px solid #334155;
  overflow: hidden;
  position: relative;
}

.viewer-toolbar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 0.85rem 1.25rem;
  background-color: #0f172a;
  border-bottom: 1px solid #334155;
}

.context-info {
  display: flex;
  align-items: center;
  gap: 0.75rem;
}

.context-tag {
  background-color: #312e81;
  color: #a5b4fc;
  font-weight: 700;
  font-size: 0.75rem;
  padding: 0.25rem 0.6rem;
  border-radius: 0.375rem;
  text-transform: uppercase;
}

.flags-count {
  font-size: 0.85rem;
  color: #fca5a5;
  font-weight: 600;
}

.toolbar-buttons {
  display: flex;
  gap: 0.5rem;
}

.btn-action {
  background-color: #4338ca;
  color: white;
  border: none;
  padding: 0.4rem 0.8rem;
  border-radius: 0.375rem;
  font-size: 0.8rem;
  font-weight: 600;
  cursor: pointer;
}

.btn-action.outline {
  background-color: transparent;
  border: 1px solid #475569;
  color: #cbd5e1;
}

.btn-action.danger {
  background-color: #7f1d1d;
  color: #fca5a5;
}

/* Image Stage & Dynamic Bounding Boxes */
.image-stage {
  flex: 1;
  display: flex;
  justify-content: center;
  align-items: flex-start;
  padding: 1.5rem;
  overflow: auto;
  background-color: #090d16;
}

.image-wrapper {
  position: relative;
  display: inline-block;
  max-width: 100%;
  box-shadow: 0 10px 25px -5px rgba(0, 0, 0, 0.5);
  border-radius: 0.375rem;
  overflow: hidden;
}

.document-image {
  display: block;
  max-width: 100%;
  height: auto;
  max-height: 75vh;
  user-select: none;
}

/* Bounding Box Styling */
.bounding-box {
  position: absolute;
  border: 2px solid;
  border-radius: 3px;
  cursor: pointer;
  transition: background-color 0.15s, border-color 0.15s;
  z-index: 10;
}

.bounding-box.pii {
  border-color: #3b82f6;
  background-color: rgba(59, 130, 246, 0.15);
}

.bounding-box.overreach {
  border-color: #ef4444;
  background-color: rgba(239, 68, 68, 0.2);
}

.bounding-box.financial {
  border-color: #f59e0b;
  background-color: rgba(245, 158, 11, 0.15);
}

.bounding-box.is-selected {
  outline: 3px solid #ffffff;
  z-index: 25;
}

/* Blackout State */
.bounding-box.is-obscured {
  background-color: #000000 !important;
  border-color: #000000 !important;
  display: flex;
  align-items: center;
  justify-content: center;
}

.blackout-mask {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 100%;
  height: 100%;
}

.mask-label {
  font-size: 0.6rem;
  font-weight: 800;
  color: #64748b;
  letter-spacing: 0.05em;
  user-select: none;
}

/* Interactive Hover Popover Tooltip */
.box-tooltip {
  display: none;
  position: absolute;
  bottom: calc(100% + 8px);
  left: 0;
  background-color: #0f172a;
  border: 1px solid #334155;
  border-radius: 0.5rem;
  padding: 0.6rem 0.75rem;
  min-width: 240px;
  max-width: 300px;
  color: #f8fafc;
  box-shadow: 0 10px 15px -3px rgba(0, 0, 0, 0.5);
  z-index: 30;
  pointer-events: auto;
}

.bounding-box:hover .box-tooltip,
.bounding-box.is-selected .box-tooltip {
  display: block;
}

.tooltip-header {
  display: flex;
  justify-content: space-between;
  margin-bottom: 0.25rem;
}

.category-badge {
  font-size: 0.65rem;
  font-weight: 700;
  padding: 0.15rem 0.4rem;
  border-radius: 0.25rem;
}

.category-badge.pii {
  background-color: #1e3a8a;
  color: #93c5fd;
}

.category-badge.overreach {
  background-color: #7f1d1d;
  color: #fca5a5;
}

.category-badge.financial {
  background-color: #78350f;
  color: #fde68a;
}

.severity-badge {
  font-size: 0.6rem;
  color: #94a3b8;
  font-weight: 600;
}

.tooltip-title {
  font-size: 0.8rem;
  font-weight: 700;
  margin-bottom: 0.25rem;
}

.tooltip-reason {
  font-size: 0.7rem;
  color: #cbd5e1;
  line-height: 1.3;
  margin-bottom: 0.5rem;
}

.tooltip-actions {
  display: flex;
  gap: 0.4rem;
}

.btn-tooltip-toggle {
  flex: 1;
  background-color: #3b82f6;
  color: white;
  border: none;
  padding: 0.25rem 0.5rem;
  border-radius: 0.25rem;
  font-size: 0.7rem;
  font-weight: 600;
  cursor: pointer;
}

.btn-tooltip-flag {
  background-color: #334155;
  color: #fca5a5;
  border: none;
  padding: 0.25rem 0.5rem;
  border-radius: 0.25rem;
  font-size: 0.7rem;
  cursor: pointer;
}

/* Sidebar Inspector */
.inspector-sidebar {
  background-color: #1e293b;
  border-radius: 1rem;
  border: 1px solid #334155;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.inspector-header {
  padding: 1.25rem;
  border-bottom: 1px solid #334155;
}

.inspector-header h3 {
  font-size: 1rem;
  font-weight: 700;
  margin: 0 0 0.5rem 0;
}

.summary-text {
  font-size: 0.8rem;
  color: #94a3b8;
  line-height: 1.4;
  margin: 0;
}

.box-list {
  flex: 1;
  overflow-y: auto;
  padding: 1rem;
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
}

.box-item {
  background-color: #0f172a;
  border: 1px solid #334155;
  border-radius: 0.5rem;
  padding: 0.75rem;
  transition: border-color 0.2s;
}

.box-item.active {
  border-color: #6366f1;
}

.box-item-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 0.35rem;
}

.box-item-title {
  font-size: 0.85rem;
  font-weight: 700;
}

.box-item-reason {
  font-size: 0.75rem;
  color: #94a3b8;
  margin: 0 0 0.75rem 0;
  line-height: 1.3;
}

.box-item-footer {
  display: flex;
  justify-content: space-between;
  align-items: center;
}

.switch-control {
  display: flex;
  align-items: center;
  gap: 0.4rem;
  font-size: 0.75rem;
  cursor: pointer;
  color: #cbd5e1;
}

.btn-discard {
  background: none;
  border: none;
  color: #f87171;
  font-size: 0.7rem;
  cursor: pointer;
  text-decoration: underline;
}

/* Scanning Overlay Animation */
.scanning-overlay {
  position: absolute;
  inset: 0;
  background-color: rgba(15, 23, 42, 0.75);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
}

.scanner-laser {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  height: 3px;
  background: linear-gradient(90deg, transparent, #6366f1, #ec4899, transparent);
  box-shadow: 0 0 15px #6366f1;
  animation: scanMove 2s infinite ease-in-out;
}

@keyframes scanMove {
  0% { top: 0%; }
  50% { top: 100%; }
  100% { top: 0%; }
}

.scanner-card {
  background-color: #1e293b;
  padding: 1.5rem 2rem;
  border-radius: 0.75rem;
  text-align: center;
  border: 1px solid #475569;
}

.spinner {
  width: 32px;
  height: 32px;
  border: 3px solid #334155;
  border-top-color: #6366f1;
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
  margin: 0 auto 0.75rem auto;
}

@keyframes spin {
  to { transform: rotate(360deg); }
}

.btn-action.save {
	background-color: #059669;
	color: #ffffff;
}

.btn-action.save:hover {
	background-color: #047857;
}
</style>
