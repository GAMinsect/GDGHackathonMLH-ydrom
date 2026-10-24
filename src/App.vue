<script setup>
import { ref, reactive, computed, onMounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { GoogleGenAI } from "@google/genai";
import * as pdfjsLib from "pdfjs-dist/legacy/build/pdf.mjs";
import pdfWorkerUrl from "pdfjs-dist/legacy/build/pdf.worker.min.mjs?url";
import { PDFDocument } from "pdf-lib";

pdfjsLib.GlobalWorkerOptions.workerSrc = pdfWorkerUrl;

// Limiti per il rendering dei PDF
const MAX_PDF_PAGES = 30;
const PDF_MAX_SCALE = 2.5;
const PDF_MAX_SIDE_PX = 2400;
const PDF_STEP_TIMEOUT_MS = 60000; // oltre questo tempo un passo del PDF viene considerato bloccato

// Application State
const isApiReady = ref(false);
const apiKeyError = ref("");
const isScanning = ref(false);
const scanStatus = ref("");
const imageUrl = ref("");
const imageFile = ref(null);
const selectedBoxId = ref(null);
const imageWrapperRef = ref(null);
const pages = ref([]); // [{ dataUrl, pdfWidth?, pdfHeight? }]
const currentPage = ref(0);
const sourceType = ref("image"); // "image" | "pdf"
const failedPages = ref([]); // indici (0-based) delle pagine la cui scansione è fallita

// AI Instance
let ai = null;

// Scan Results
const detectedContext = ref("");
const documentSummary = ref("");
const boxes = ref([]);
const boxStates = reactive({});

// Drag / Resize / Draw State
const interaction = ref(null);
let suppressNextClick = false;
let manualBoxCounter = 1;
let hoverClearTimer = null;

// Gestione fluida dell'hover tra rettangolo e popup (evita che sparisca spostando il mouse)
function onBoxMouseEnter(boxId) {
	if (hoverClearTimer) {
		clearTimeout(hoverClearTimer);
		hoverClearTimer = null;
	}
	selectedBoxId.value = boxId;
}

function onBoxMouseLeave(boxId) {
	if (hoverClearTimer) clearTimeout(hoverClearTimer);
	hoverClearTimer = setTimeout(() => {
		if (selectedBoxId.value === boxId) {
			selectedBoxId.value = null;
		}
	}, 180);
}

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

// Keep only well-formed boxes and normalize coordinates to 0-1000
function normalizeBoxes(rawBoxes) {
	const CATEGORIES = ["PII", "OVERREACH", "FINANCIAL", "MANUAL"];
	return (Array.isArray(rawBoxes) ? rawBoxes : [])
		.filter(
			(b) =>
				Array.isArray(b?.box_2d) &&
				b.box_2d.length === 4 &&
				b.box_2d.every((n) => Number.isFinite(Number(n)))
		)
		.map((b, i) => {
			let [ymin, xmin, ymax, xmax] = b.box_2d.map(Number);
			if (Math.max(ymin, xmin, ymax, xmax) <= 1.0) {
				ymin *= 1000;
				xmin *= 1000;
				ymax *= 1000;
				xmax *= 1000;
			}
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
	event.target.value = "";
	if (!file) return;
	loadFile(file);
}

function onFileDrop(event) {
	event.preventDefault();
	const file = event.dataTransfer.files?.[0];
	if (!file) return;
	loadFile(file);
}

function isPdfFile(file) {
	return file.type === "application/pdf" || file.name?.toLowerCase().endsWith(".pdf");
}

function loadFile(file) {
	if (isPdfFile(file)) {
		loadPdfFile(file);
	} else if (file.type.startsWith("image/")) {
		loadImageFile(file);
	} else {
		alert("Formato non supportato. Carica un'immagine (PNG, JPG, WEBP) o un file PDF.");
	}
}

function loadImageFile(file) {
	imageFile.value = file;
	const reader = new FileReader();
	reader.onload = (e) => {
		pages.value = [{ dataUrl: e.target.result }];
		sourceType.value = "image";
		failedPages.value = [];
		currentPage.value = 0;
		imageUrl.value = e.target.result;
		scanPages();
	};
	reader.readAsDataURL(file);
}

// Evita che un passo di pdf.js resti appeso all'infinito
function withTimeout(promise, ms, message) {
	let timer;
	const timeout = new Promise((_, reject) => {
		timer = setTimeout(() => reject(new Error(message)), ms);
	});
	return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

// Renderizza ogni pagina del PDF in un'immagine PNG, poi avvia la scansione
// Renderizza ogni pagina del PDF in un'immagine PNG, poi avvia la scansione
async function loadPdfFile(file) {
	isScanning.value = true;
	scanStatus.value = "Apertura del PDF...";

	let loadingTask = null; // Dichiarato qui per accedervi nel finally
	let pdf = null;
	const rendered = [];
	try {
		const data = new Uint8Array(await file.arrayBuffer());
		loadingTask = pdfjsLib.getDocument({ data, isEvalSupported: false });

		pdf = await withTimeout(
			loadingTask.promise,
			PDF_STEP_TIMEOUT_MS,
			"Timeout nell'apertura del PDF."
		);
		console.log(`PDF aperto: ${pdf.numPages} pagine`);

		if (pdf.numPages > MAX_PDF_PAGES) {
			throw new Error(
				`Il PDF ha ${pdf.numPages} pagine, il limite è ${MAX_PDF_PAGES}. Dividilo in più file.`
			);
		}

		for (let n = 1; n <= pdf.numPages; n++) {
			scanStatus.value = `Rendering pagina ${n} di ${pdf.numPages}...`;

			// Micro-pausa per far respirare Vue ed evitare il blocco dell'interfaccia
			await new Promise(resolve => setTimeout(resolve, 50));

			const page = await withTimeout(
				pdf.getPage(n),
				PDF_STEP_TIMEOUT_MS,
				`Timeout nel caricamento della pagina ${n}.`
			);
			const base = page.getViewport({ scale: 1 });
			const scale = Math.min(PDF_MAX_SCALE, PDF_MAX_SIDE_PX / Math.max(base.width, base.height));
			const viewport = page.getViewport({ scale });

			const canvas = document.createElement("canvas");
			canvas.width = Math.floor(viewport.width);
			canvas.height = Math.floor(viewport.height);

			const renderTask = page.render({
				canvas,
				canvasContext: canvas.getContext("2d"),
				viewport,
			});
			try {
				await withTimeout(
					renderTask.promise,
					PDF_STEP_TIMEOUT_MS,
					`Timeout nel rendering della pagina ${n}.`
				);
			} catch (err) {
				renderTask.cancel();
				throw err;
			}
			console.log(`Pagina ${n}/${pdf.numPages} renderizzata (${canvas.width}x${canvas.height})`);

			rendered.push({
				dataUrl: canvas.toDataURL("image/png"),
				pdfWidth: base.width,
				pdfHeight: base.height,
			});

			// Libera la memoria della singola pagina
			page.cleanup();
		}
	} catch (error) {
		console.error("Errore durante la lettura del PDF:", error);
		isScanning.value = false;
		scanStatus.value = "";
		alert("Impossibile leggere il PDF: " + (error?.message || error));
		return;
	} finally {
		// FIX: Chiamare destroy() sul loadingTask invece che su pdf[cite: 1]
		if (loadingTask) loadingTask.destroy();
	}

	imageFile.value = file;
	pages.value = rendered;
	sourceType.value = "pdf";
	failedPages.value = [];
	currentPage.value = 0;
	imageUrl.value = rendered[0].dataUrl;
	await scanPages();
}

// Cambio pagina nel viewer
function goToPage(index) {
	if (index < 0 || index >= pages.value.length) return;
	currentPage.value = index;
	imageUrl.value = pages.value[index].dataUrl;
	selectedBoxId.value = null;
}

function boxPageNumber(box) {
	return (box.page ?? 0) + 1;
}

function goToBoxPage(box) {
	const index = box.page ?? 0;
	if (index !== currentPage.value) {
		goToPage(index);
		selectedBoxId.value = box.id;
	}
}

// Invia una singola pagina (immagine) al modello, con fallback tra i modelli disponibili
async function analyzePage(dataUrl) {
	const [header, rawBase64] = dataUrl.split(",");
	const mimeType = header.match(/^data:(.*?);base64$/)?.[1] || "image/jpeg";

	let response;
	let lastError;
	for (const model of GEMMA_MODELS) {
		try {
			response = await ai.models.generateContent({
				model,
				contents: [
					{ inlineData: { data: rawBase64, mimeType } },
					{ text: SCANNER_PROMPT },
				],
				config: { responseMimeType: "application/json" },
			});
			console.log(`Scansione completata con ${model}`);
			break;
		} catch (err) {
			console.warn(`Modello ${model} non disponibile:`, err);
			lastError = err;
		}
	}
	if (!response) throw lastError;
	return parseModelJson(response.text);
}

// Scansiona tutte le pagine caricate (1 per le immagini, N per i PDF)
async function scanPages() {
	if (!ai || !pages.value.length) {
		isScanning.value = false;
		scanStatus.value = "";
		return;
	}

	isScanning.value = true;
	boxes.value = [];
	failedPages.value = [];

	const total = pages.value.length;
	const collected = [];
	const failed = [];
	let context = "";
	let summary = "";
	let lastErrorMessage = "";

	try {
		for (let i = 0; i < total; i++) {
			scanStatus.value =
				total > 1
					? `Analisi pagina ${i + 1} di ${total}...`
					: "Rilevamento aree critiche e dati sensibili...";
			try {
				const result = await analyzePage(pages.value[i].dataUrl);
				if (!context) {
					context = result.detected_context || "";
					summary = result.summary || "";
				}
				normalizeBoxes(result.boxes).forEach((b) => {
					collected.push({ ...b, id: `p${i}_${b.id}`, page: i });
				});
			} catch (err) {
				console.error(`Errore durante la scansione della pagina ${i + 1}:`, err);
				lastErrorMessage = err?.message || String(err);
				failed.push(i);
			}
		}

		detectedContext.value = context || "Documento Contrattuale";
		documentSummary.value =
			(summary || "Analisi completata.") +
			(total > 1 ? ` (${total} pagine analizzate)` : "");
		boxes.value = collected;
		failedPages.value = failed;

		collected.forEach((b) => {
			boxStates[b.id] = {
				obscured: b.default_obscure ?? true,
				hallucination: false,
			};
		});

		if (failed.length > 0) {
			const list = failed.map((i) => i + 1).join(", ");
			alert(
				`Scansione non riuscita per ${failed.length === 1 ? "la pagina" : "le pagine"}: ${list}.\n` +
				`Controllale e aggiungi le censure a mano prima di salvare.\n\n` +
				`Dettaglio: ${lastErrorMessage}`
			);
		}
	} finally {
		isScanning.value = false;
		scanStatus.value = "";
	}
}

// Convert coordinates (0-1000) to CSS percentage
function getBoxStyle(box) {
	const [ymin, xmin, ymax, xmax] = box.box_2d;
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

// Se il box è molto vicino al bordo superiore dell'immagine, mostra il tooltip sotto
function isNearTop(box) {
	const ymin = box.box_2d[0];
	const normalizedY = ymin <= 1.0 ? ymin * 1000 : ymin;
	return normalizedY < 160;
}

// Add a new censoring rectangle via button
function addManualBox() {
	const offset = ((manualBoxCounter - 1) % 6) * 25;
	const newId = `manual_${Date.now()}_${manualBoxCounter}`;
	const newBox = {
		id: newId,
		page: currentPage.value,
		box_2d: [380 + offset, 300 + offset, 440 + offset, 700 + offset],
		label: `Censura Manuale #${manualBoxCounter}`,
		category: "MANUAL",
		severity: "HIGH",
		reason: "Area aggiunta manualmente dall'utente per l'oscuramento.",
		default_obscure: true,
	};
	manualBoxCounter++;
	boxes.value.push(newBox);
	boxStates[newId] = { obscured: true, hallucination: false };
	selectedBoxId.value = newId;
}

// Mouse Dragging, Resizing & Drawing on Image
function startBoxMove(event, box) {
	if (event.button !== 0 || !imageWrapperRef.value) return;
	const rect = imageWrapperRef.value.getBoundingClientRect();
	selectedBoxId.value = box.id;

	interaction.value = {
		type: "move",
		box,
		rect,
		startX: event.clientX,
		startY: event.clientY,
		startBox2d: [...box.box_2d],
		moved: false,
	};

	window.addEventListener("mousemove", onGlobalMouseMove);
	window.addEventListener("mouseup", onGlobalMouseUp);
}

function startBoxResize(event, box, handle) {
	if (event.button !== 0 || !imageWrapperRef.value) return;
	const rect = imageWrapperRef.value.getBoundingClientRect();
	selectedBoxId.value = box.id;

	interaction.value = {
		type: "resize",
		box,
		handle,
		rect,
		startX: event.clientX,
		startY: event.clientY,
		startBox2d: [...box.box_2d],
		moved: false,
	};

	window.addEventListener("mousemove", onGlobalMouseMove);
	window.addEventListener("mouseup", onGlobalMouseUp);
}

// Draw a new box by clicking and dragging directly on the image background
function startDrawNewBox(event) {
	if (event.button !== 0 || !imageWrapperRef.value) return;
	if (event.target.closest(".bounding-box")) return;

	const rect = imageWrapperRef.value.getBoundingClientRect();
	const startXNorm = Math.max(0, Math.min(1000, ((event.clientX - rect.left) / rect.width) * 1000));
	const startYNorm = Math.max(0, Math.min(1000, ((event.clientY - rect.top) / rect.height) * 1000));

	interaction.value = {
		type: "draw",
		rect,
		startX: event.clientX,
		startY: event.clientY,
		startXNorm,
		startYNorm,
		box: null,
		moved: false,
	};

	window.addEventListener("mousemove", onGlobalMouseMove);
	window.addEventListener("mouseup", onGlobalMouseUp);
}

function onGlobalMouseMove(event) {
	const state = interaction.value;
	if (!state) return;

	const pixelDx = event.clientX - state.startX;
	const pixelDy = event.clientY - state.startY;

	if (Math.abs(pixelDx) > 3 || Math.abs(pixelDy) > 3) {
		state.moved = true;
	}

	const dx = (pixelDx / state.rect.width) * 1000;
	const dy = (pixelDy / state.rect.height) * 1000;
	const MIN_SIZE = 12;

	if (state.type === "move") {
		const [y1, x1, y2, x2] = state.startBox2d;
		const h = y2 - y1;
		const w = x2 - x1;

		const newY1 = Math.max(0, Math.min(1000 - h, y1 + dy));
		const newX1 = Math.max(0, Math.min(1000 - w, x1 + dx));

		state.box.box_2d = [newY1, newX1, newY1 + h, newX1 + w];
	} else if (state.type === "resize") {
		let [y1, x1, y2, x2] = state.startBox2d;
		const h = state.handle;

		if (h.includes("n")) y1 = Math.max(0, Math.min(y2 - MIN_SIZE, y1 + dy));
		if (h.includes("s")) y2 = Math.min(1000, Math.max(y1 + MIN_SIZE, y2 + dy));
		if (h.includes("w")) x1 = Math.max(0, Math.min(x2 - MIN_SIZE, x1 + dx));
		if (h.includes("e")) x2 = Math.min(1000, Math.max(x1 + MIN_SIZE, x2 + dx));

		state.box.box_2d = [y1, x1, y2, x2];
	} else if (state.type === "draw") {
		if (!state.moved) return;

		const curXNorm = Math.max(0, Math.min(1000, ((event.clientX - state.rect.left) / state.rect.width) * 1000));
		const curYNorm = Math.max(0, Math.min(1000, ((event.clientY - state.rect.top) / state.rect.height) * 1000));

		const ymin = Math.min(state.startYNorm, curYNorm);
		const xmin = Math.min(state.startXNorm, curXNorm);
		const ymax = Math.max(state.startYNorm, curYNorm);
		const xmax = Math.max(state.startXNorm, curXNorm);

		if (!state.box) {
			const newId = `manual_${Date.now()}_${manualBoxCounter}`;
			const newBox = reactive({
				id: newId,
				page: currentPage.value,
				box_2d: [ymin, xmin, Math.max(ymin + MIN_SIZE, ymax), Math.max(xmin + MIN_SIZE, xmax)],
				label: `Censura Manuale #${manualBoxCounter}`,
				category: "MANUAL",
				severity: "HIGH",
				reason: "Area disegnata manualmente dall'utente.",
				default_obscure: true,
			});
			manualBoxCounter++;
			boxes.value.push(newBox);
			boxStates[newId] = { obscured: true, hallucination: false };
			selectedBoxId.value = newId;
			state.box = newBox;
		} else {
			state.box.box_2d = [ymin, xmin, Math.max(ymin + MIN_SIZE, ymax), Math.max(xmin + MIN_SIZE, xmax)];
		}
	}
}

function onGlobalMouseUp() {
	if (interaction.value?.moved) {
		suppressNextClick = true;
		setTimeout(() => {
			suppressNextClick = false;
		}, 50);
	}
	interaction.value = null;
	window.removeEventListener("mousemove", onGlobalMouseMove);
	window.removeEventListener("mouseup", onGlobalMouseUp);
}

function onBoxClick(boxId) {
	if (suppressNextClick) return;
	toggleObscure(boxId);
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
		if (selectedBoxId.value === boxId) {
			selectedBoxId.value = null;
		}
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
	pages.value = [];
	currentPage.value = 0;
	sourceType.value = "image";
	failedPages.value = [];
	selectedBoxId.value = null;
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

	const canvas = document.createElement("canvas");
	canvas.width = 800;
	canvas.height = 1100;
	const ctx = canvas.getContext("2d");

	ctx.fillStyle = "#ffffff";
	ctx.fillRect(0, 0, 800, 1100);

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
	pages.value = [{ dataUrl: imageUrl.value }];
	currentPage.value = 0;
	sourceType.value = "image";
	failedPages.value = [];

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

// Box della sola pagina visualizzata
const pageBoxes = computed(() => {
	return activeBoxes.value.filter((b) => (b.page ?? 0) === currentPage.value);
});

// Numero di aree per ogni pagina (per il selettore pagine)
const pageCounts = computed(() => {
	return pages.value.map((_, i) => activeBoxes.value.filter((b) => (b.page ?? 0) === i).length);
});

const criticalCount = computed(() => {
	return activeBoxes.value.filter((b) => b.severity === "CRITICAL").length;
});

function dataUrlToBytes(dataUrl) {
	const binary = atob(dataUrl.split(",")[1]);
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
	return bytes;
}

function bytesToBase64(bytes) {
	let binary = "";
	const chunk = 0x8000;
	for (let i = 0; i < bytes.length; i += chunk) {
		binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
	}
	return btoa(binary);
}

// Disegna i rettangoli neri sulla pagina indicata e restituisce il canvas
async function renderCensoredCanvas(pageIndex) {
	const img = new Image();
	img.src = pages.value[pageIndex].dataUrl;
	await new Promise((resolve, reject) => {
		img.onload = resolve;
		img.onerror = reject;
	});

	const canvas = document.createElement("canvas");
	canvas.width = img.naturalWidth || img.width;
	canvas.height = img.naturalHeight || img.height;
	const ctx = canvas.getContext("2d");

	ctx.drawImage(img, 0, 0);

	ctx.fillStyle = "#000000";
	activeBoxes.value
		.filter((box) => (box.page ?? 0) === pageIndex)
		.forEach((box) => {
			if (boxStates[box.id]?.obscured) {
				const [ymin, xmin, ymax, xmax] = box.box_2d;
				const factor = ymax <= 1.0 ? 1.0 : 0.001;

				const x = xmin * factor * canvas.width;
				const y = ymin * factor * canvas.height;
				const w = (xmax - xmin) * factor * canvas.width;
				const h = (ymax - ymin) * factor * canvas.height;

				ctx.fillRect(x, y, w, h);
			}
		});

	return canvas;
}

// Costruisce un PDF fatto di sole immagini: il testo originale non resta nel file,
// quindi i dati oscurati non sono recuperabili (niente testo selezionabile sotto i rettangoli).
async function saveCensoredPdf() {
	const pdfDoc = await PDFDocument.create();

	for (let i = 0; i < pages.value.length; i++) {
		const canvas = await renderCensoredCanvas(i);
		const png = await pdfDoc.embedPng(dataUrlToBytes(canvas.toDataURL("image/png")));
		const { pdfWidth, pdfHeight } = pages.value[i];
		const page = pdfDoc.addPage([pdfWidth, pdfHeight]);
		page.drawImage(png, { x: 0, y: 0, width: pdfWidth, height: pdfHeight });
	}

	const base64Pdf = bytesToBase64(await pdfDoc.save());
	const savedPath = await invoke("save_censored_pdf", { base64Pdf });

	if (savedPath) {
		alert(`PDF censurato salvato con successo in:\n${savedPath}`);
	}
}

async function saveCensoredDocument() {
	if (!pages.value.length) return;

	try {
		if (sourceType.value === "pdf") {
			await saveCensoredPdf();
			return;
		}

		const canvas = await renderCensoredCanvas(0);
		const censoredDataUrl = canvas.toDataURL("image/png");
		const savedPath = await invoke("save_censored_image", {
			base64Image: censoredDataUrl,
		});

		if (savedPath) {
			alert(`Immagine censurata salvata con successo in:\n${savedPath}`);
		}
	} catch (error) {
		console.error("Errore durante il salvataggio:", error);
		alert("Errore durante il salvataggio: " + error);
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
				<h2>Carica o Trascina un Contratto (foto o PDF)</h2>
				<p>Supporta foto, screenshot, scansioni (PNG, JPG, WEBP) e PDF multipagina</p>

				<p v-if="isScanning" class="dropzone-status">{{ scanStatus }}</p>
				<div class="upload-actions">
					<label class="btn-primary">
						📁 Scegli File
						<input
							type="file"
							accept="image/*,application/pdf,.pdf"
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
              🚨 {{ activeBoxes.length }} aree ({{ criticalCount }} critiche)
            </span>
					</div>

					<div class="toolbar-buttons">
						<button
							class="btn-action add-box"
							@click="addManualBox"
							title="Aggiungi un nuovo rettangolo di censura (puoi anche disegnarlo trascinando col mouse sul documento)"
						>
							➕ Aggiungi Censura
						</button>
						<button class="btn-action" @click="obscureAll" title="Nascondi tutte le aree suggerite">
							🛡️ Oscura Tutto
						</button>
						<button class="btn-action outline" @click="revealAll" title="Mostra tutte le aree">
							👁️ Mostra Tutto
						</button>
						<button class="btn-action save" @click="saveCensoredDocument" title="Esporta e salva il documento censurato">
							💾 Salva Censurato
						</button>
						<button class="btn-action danger" @click="resetAll">
							✕ Nuovo Documento
						</button>
					</div>
				</div>

				<!-- Hint Banner -->
				<div class="editor-hint">
					💡 <strong>Suggerimento:</strong> Trascina i rettangoli per spostarli, usa i quadratini sui bordi per ridimensionarli, oppure clicca e trascina direttamente sul documento per disegnare una nuova censura.
				</div>

				<!-- Page Selector (PDF multipagina) -->
				<div v-if="pages.length > 1" class="pager">
					<button
						class="btn-action outline"
						:disabled="currentPage === 0"
						@click="goToPage(currentPage - 1)"
					>
						◀
					</button>
					<div class="page-chips">
						<button
							v-for="(p, i) in pages"
							:key="'page_' + i"
							:class="[
				'page-chip',
				{
					active: i === currentPage,
					flagged: pageCounts[i] > 0,
					failed: failedPages.includes(i),
				},
			]"
							:title="failedPages.includes(i) ? 'Scansione non riuscita' : `${pageCounts[i]} aree`"
							@click="goToPage(i)"
						>
							{{ i + 1 }}
						</button>
					</div>
					<button
						class="btn-action outline"
						:disabled="currentPage === pages.length - 1"
						@click="goToPage(currentPage + 1)"
					>
						▶
					</button>
					<span class="pager-label">Pagina {{ currentPage + 1 }} di {{ pages.length }}</span>
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
					<div
						ref="imageWrapperRef"
						class="image-wrapper"
						@mousedown="startDrawNewBox"
					>
						<img
							:src="imageUrl"
							alt="Documento Contrattuale"
							class="document-image"
							draggable="false"
						/>

						<!-- Dynamic Bounding Boxes -->
						<div
							v-for="box in pageBoxes"
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
							@mouseenter="onBoxMouseEnter(box.id)"
							@mouseleave="onBoxMouseLeave(box.id)"
							@mousedown.stop="startBoxMove($event, box)"
							@click.stop="onBoxClick(box.id)"
						>
							<!-- Obscured Blackout Mask -->
							<div v-if="boxStates[box.id]?.obscured" class="blackout-mask">
								<span class="mask-label">OSCURATO</span>
							</div>

							<!-- 8 Resize Handles (Corners & Edges) -->
							<span
								v-for="dir in ['nw', 'n', 'ne', 'e', 'se', 's', 'sw', 'w']"
								:key="dir"
								:class="['resize-handle', `handle-${dir}`]"
								@mousedown.stop.prevent="startBoxResize($event, box, dir)"
								@click.stop
							></span>

							<!-- Interactive Hover Tooltip -->
							<div
								:class="['box-tooltip', { 'tooltip-below': isNearTop(box) }]"
								@mouseenter="onBoxMouseEnter(box.id)"
								@mouseleave="onBoxMouseLeave(box.id)"
								@mousedown.stop
								@click.stop
							>
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
										@click.stop="toggleObscure(box.id)"
									>
										{{ boxStates[box.id]?.obscured ? "👁️ Mostra Testo" : "🛡️ Oscura" }}
									</button>
									<button
										class="btn-tooltip-flag"
										@click.stop="flagHallucination(box.id)"
										title="Rimuovi questo rettangolo"
									>
										🗑️ Rimuovi
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
					<h3>Rilevamenti Critici & Censure</h3>
					<p class="summary-text">{{ documentSummary }}</p>
				</div>

				<div class="box-list">
					<div
						v-for="box in activeBoxes"
						:key="'list_' + box.id"
						@click="goToBoxPage(box)"
						:class="[
              'box-item',
              box.category.toLowerCase(),
              {
                'active': selectedBoxId === box.id,
                'obscured': boxStates[box.id]?.obscured,
              },
            ]"
						@mouseenter="onBoxMouseEnter(box.id)"
						@mouseleave="onBoxMouseLeave(box.id)"
					>
						<div class="box-item-header">
							<span class="box-item-title">{{ box.label }}<small v-if="pages.length > 1" class="box-page-tag"> · pag. {{ boxPageNumber(box) }}</small></span>
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
								title="Rimuovi questo rettangolo"
							>
								Rimuovi
							</button>
						</div>
					</div>
				</div>
			</aside>
		</section>
	</main>
</template>

<!-- Reset globale per eliminare il margine bianco e bloccare lo scroll della finestra -->
<!-- Reset globale per eliminare il margine bianco e bloccare lo scroll della finestra -->
<style>
*,
*::before,
*::after {
	box-sizing: border-box;
}

html,
body,
#app {
	margin: 0;
	padding: 0;
	width: 100%;
	height: 100%;
	overflow: hidden;
	background-color: #003B3E; /* Sfondo principale molto più scuro */
}
</style>

<style scoped>
/* App Shell & Header */
.app-shell {
	height: 100vh;
	max-height: 100vh;
	width: 100vw;
	overflow: hidden;
	display: flex;
	flex-direction: column;
	background-color: #003B3E; /* Sfondo principale molto più scuro */
	color: #FFFFFF;
	font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}

.app-header {
	flex-shrink: 0;
	display: flex;
	justify-content: space-between;
	align-items: center;
	padding: 0.85rem 1.5rem;
	background-color: rgba(28, 28, 28, 0.15); /* Leggera ombra per staccare dal blu */
	border-bottom: 1px solid #A2C4C9; /* Blu main per i bordi */
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
	color: #FFFFFF;
}

.brand-badge {
	background-color: #A2C4C9; /* Blu main */
	color: #FFFFFF;
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
	background-color: #85D5A4; /* Verde conferma */
	color: #FFFFFF;
}

.status-pill.error {
	background-color: #D26031; /* Rosso errore/pericolo */
	color: #FFFFFF;
}

.status-pill.loading {
	background-color: #A2C4C9; /* Blu main */
	color: #FFFFFF;
}

/* Dropzone Screen */
.upload-container {
	flex: 1;
	min-height: 0;
	display: flex;
	align-items: center;
	justify-content: center;
	padding: 2rem;
}

.dropzone {
	max-width: 600px;
	width: 100%;
	padding: 3.5rem 2rem;
	background-color: rgba(162, 196, 201, 0.1); /* Sfondo blu trasparente */
	border: 2px dashed #A2C4C9; /* Blu main */
	border-radius: 1.25rem;
	text-align: center;
	transition: border-color 0.2s;
}

.dropzone:hover {
	border-color: #FFF0DD; /* Bianco soft per l'hover */
}

.dropzone-icon {
	font-size: 3.5rem;
	margin-bottom: 1rem;
}

.dropzone h2 {
	font-size: 1.4rem;
	font-weight: 700;
	margin-bottom: 0.5rem;
	color: #FFFFFF;
}

.dropzone p {
	color: #FFFFFF;
	font-size: 0.9rem;
	margin-bottom: 2rem;
	opacity: 0.9;
}

.upload-actions {
	display: flex;
	justify-content: center;
	gap: 1rem;
}

.btn-primary {
	background-color: #A2C4C9; /* Blu main */
	color: #FFFFFF;
	padding: 0.75rem 1.5rem;
	border-radius: 0.5rem;
	font-weight: 600;
	cursor: pointer;
	border: none;
	transition: background-color 0.2s;
}

.btn-primary:hover {
	background-color: #FFF0DD; /* Bianco soft */
	color: #1C1C1C; /* Testo nero per contrasto sull'hover chiaro */
}

.btn-secondary {
	background-color: transparent;
	color: #FFFFFF;
	padding: 0.75rem 1.5rem;
	border-radius: 0.5rem;
	font-weight: 600;
	cursor: pointer;
	border: 1px solid #A2C4C9; /* Blu main */
	transition: background-color 0.2s;
}

.btn-secondary:hover {
	background-color: rgba(162, 196, 201, 0.2);
}

/* Inspection Layout */
.inspection-layout {
	flex: 1;
	min-height: 0;
	display: grid;
	grid-template-columns: 1fr 380px;
	gap: 1.25rem;
	padding: 1.25rem;
	overflow: hidden;
}

.viewer-card {
	min-height: 0;
	height: 100%;
	display: flex;
	flex-direction: column;
	background-color: rgba(162, 196, 201, 0.1); /* Toni di blu */
	border-radius: 1rem;
	border: 1px solid #A2C4C9;
	overflow: hidden;
	position: relative;
}

.viewer-toolbar {
	flex-shrink: 0;
	display: flex;
	justify-content: space-between;
	align-items: center;
	padding: 0.75rem 1.25rem;
	background-color: rgba(28, 28, 28, 0.15);
	border-bottom: 1px solid #A2C4C9;
	flex-wrap: wrap;
	gap: 0.5rem;
}

.editor-hint {
	flex-shrink: 0;
	background-color: rgba(28, 28, 28, 0.2);
	color: #FFFFFF;
	font-size: 0.75rem;
	padding: 0.45rem 1.25rem;
	border-bottom: 1px solid #A2C4C9;
}

.context-info {
	display: flex;
	align-items: center;
	gap: 0.75rem;
}

.context-tag {
	background-color: #A2C4C9; /* Blu main */
	color: #FFFFFF;
	font-weight: 700;
	font-size: 0.75rem;
	padding: 0.25rem 0.6rem;
	border-radius: 0.375rem;
	text-transform: uppercase;
}

.flags-count {
	font-size: 0.85rem;
	color: #D26031; /* Rosso per il pericolo/allarmi */
	font-weight: 600;
}

.toolbar-buttons {
	display: flex;
	gap: 0.5rem;
	flex-wrap: wrap;
}

.btn-action {
	background-color: #A2C4C9; /* Blu main */
	color: #FFFFFF;
	border: none;
	padding: 0.4rem 0.8rem;
	border-radius: 0.375rem;
	font-size: 0.8rem;
	font-weight: 600;
	cursor: pointer;
}

.btn-action.add-box {
	background-color: #A2C4C9;
}

.btn-action.outline {
	background-color: transparent;
	border: 1px solid #FFF0DD; /* Bianco soft */
	color: #FFFFFF;
}

.btn-action.danger {
	background-color: #D26031; /* Rosso negazione/eliminazione */
	color: #FFFFFF;
}

.btn-action.save {
	background-color: #85D5A4; /* Verde conferma/salvataggio */
	color: #FFFFFF;
}

/* Pager */
.dropzone .dropzone-status {
	margin: 0 0 1.25rem 0;
	color: #FFF0DD;
	font-weight: 600;
}

.pager {
	flex-shrink: 0;
	display: flex;
	align-items: center;
	gap: 0.6rem;
	padding: 0.4rem 1.25rem;
	background-color: rgba(28, 28, 28, 0.15);
	border-bottom: 1px solid #A2C4C9;
}

.pager-label {
	flex-shrink: 0;
	font-size: 0.75rem;
	color: #FFFFFF;
}

.page-chips {
	flex: 1;
	min-width: 0;
	display: flex;
	gap: 0.3rem;
	overflow-x: auto;
	padding: 0.15rem 0;
}

.page-chip {
	flex-shrink: 0;
	min-width: 2rem;
	padding: 0.25rem 0.5rem;
	background-color: transparent;
	border: 1px solid #A2C4C9;
	border-radius: 0.375rem;
	color: #FFFFFF;
	font-size: 0.75rem;
	font-weight: 600;
	cursor: pointer;
}

.page-chip.flagged {
	border-color: #D26031; /* Rosso pericolo */
}

.page-chip.failed {
	border-color: #D26031;
	color: #D26031;
}

.page-chip.active {
	background-color: #FFF0DD; /* Bianco soft per la pagina attiva */
	border-color: #FFF0DD;
	color: #1C1C1C; /* Testo nero su fondo chiaro */
}

.btn-action:disabled {
	opacity: 0.4;
	cursor: not-allowed;
}

.box-page-tag {
	color: #FFFFFF;
	font-weight: 500;
	opacity: 0.8;
}

/* Image Stage & Bounding Boxes */
.image-stage {
	flex: 1;
	min-height: 0;
	display: flex;
	justify-content: center;
	align-items: center;
	padding: 1.25rem;
	overflow: auto;
	background-color: rgba(28, 28, 28, 0.4);
}

.image-wrapper {
	position: relative;
	display: inline-block;
	max-width: 100%;
	box-shadow: 0 10px 25px -5px rgba(28, 28, 28, 0.5);
	border-radius: 0.375rem;
	cursor: crosshair;
	user-select: none;
}

.document-image {
	display: block;
	max-width: 100%;
	height: auto;
	max-height: calc(100vh - 220px);
	user-select: none;
	pointer-events: none;
}

/* Bounding Box Styling */
.bounding-box {
	position: absolute;
	border: 2px solid;
	border-radius: 3px;
	cursor: move;
	transition: background-color 0.15s, border-color 0.15s;
	z-index: 10;
	box-sizing: border-box;
}

.bounding-box.pii {
	border-color: #A2C4C9; /* Blu main */
	background-color: rgba(162, 196, 201, 0.2);
}

.bounding-box.overreach {
	border-color: #D26031; /* Rosso pericolo */
	background-color: rgba(210, 96, 49, 0.2);
}

.bounding-box.financial {
	border-color: #FFF0DD; /* Bianco soft per attenzione */
	background-color: rgba(255, 240, 221, 0.2);
}

.bounding-box.manual {
	border-color: #85D5A4; /* Verde per utente */
	background-color: rgba(133, 213, 164, 0.2);
}

.bounding-box.is-selected {
	outline: 2px solid #FFFFFF;
	z-index: 30;
}

/* Blackout State - Censure Nere */
.bounding-box.is-obscured {
	background-color: #1C1C1C !important; /* Nero */
	border-color: #1C1C1C !important; /* Nero */
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
	overflow: hidden;
	pointer-events: none;
}

.mask-label {
	font-size: 0.6rem;
	font-weight: 800;
	color: #FFFFFF;
	letter-spacing: 0.05em;
	user-select: none;
}

/* Resize Handles */
.resize-handle {
	position: absolute;
	width: 9px;
	height: 9px;
	background-color: #FFFFFF;
	border: 1.5px solid #1C1C1C;
	border-radius: 2px;
	opacity: 0;
	transition: opacity 0.15s;
	z-index: 35;
}

.bounding-box:hover .resize-handle,
.bounding-box.is-selected .resize-handle {
	opacity: 1;
}

.handle-nw { top: -5px; left: -5px; cursor: nwse-resize; }
.handle-n  { top: -5px; left: calc(50% - 4.5px); cursor: ns-resize; }
.handle-ne { top: -5px; right: -5px; cursor: nesw-resize; }
.handle-e  { top: calc(50% - 4.5px); right: -5px; cursor: ew-resize; }
.handle-se { bottom: -5px; right: -5px; cursor: nwse-resize; }
.handle-s  { bottom: -5px; left: calc(50% - 4.5px); cursor: ns-resize; }
.handle-sw { bottom: -5px; left: -5px; cursor: nesw-resize; }
.handle-w  { top: calc(50% - 4.5px); left: -5px; cursor: ew-resize; }

/* Interactive Hover Popover Tooltip */
.box-tooltip {
	display: none;
	position: absolute;
	bottom: calc(100% + 8px);
	left: 0;
	background-color: #1C1C1C; /* Nero per il tooltip */
	border: 1px solid #A2C4C9; /* Bordo blu main */
	border-radius: 0.5rem;
	padding: 0.6rem 0.75rem;
	min-width: 240px;
	max-width: 300px;
	color: #FFFFFF; /* Testo bianco */
	box-shadow: 0 10px 15px -3px rgba(28, 28, 28, 0.8);
	z-index: 45;
	pointer-events: auto;
	cursor: default;
}

.box-tooltip::after {
	content: "";
	position: absolute;
	top: 100%;
	left: 0;
	right: 0;
	height: 14px;
	background: transparent;
}

.box-tooltip.tooltip-below {
	bottom: auto;
	top: calc(100% + 8px);
}

.box-tooltip.tooltip-below::after {
	top: auto;
	bottom: 100%;
	height: 14px;
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
	background-color: #A2C4C9;
	color: #1C1C1C;
}

.category-badge.overreach {
	background-color: #D26031;
	color: #FFFFFF;
}

.category-badge.financial {
	background-color: #FFF0DD;
	color: #1C1C1C;
}

.category-badge.manual {
	background-color: #85D5A4;
	color: #1C1C1C;
}

.severity-badge {
	font-size: 0.6rem;
	color: #FFF0DD;
	font-weight: 600;
}

.tooltip-title {
	font-size: 0.8rem;
	font-weight: 700;
	margin-bottom: 0.25rem;
}

.tooltip-reason {
	font-size: 0.7rem;
	color: #FFFFFF;
	line-height: 1.3;
	margin-bottom: 0.5rem;
}

.tooltip-actions {
	display: flex;
	gap: 0.4rem;
}

.btn-tooltip-toggle {
	flex: 1;
	background-color: #A2C4C9;
	color: #1C1C1C;
	border: none;
	padding: 0.3rem 0.5rem;
	border-radius: 0.25rem;
	font-size: 0.7rem;
	font-weight: 600;
	cursor: pointer;
}

.btn-tooltip-flag {
	background-color: #D26031; /* Rosso */
	color: #FFFFFF;
	border: none;
	padding: 0.3rem 0.5rem;
	border-radius: 0.25rem;
	font-size: 0.7rem;
	cursor: pointer;
}

/* Sidebar Inspector */
.inspector-sidebar {
	min-height: 0;
	height: 100%;
	background-color: rgba(162, 196, 201, 0.1);
	border-radius: 1rem;
	border: 1px solid #A2C4C9;
	display: flex;
	flex-direction: column;
	overflow: hidden;
}

.inspector-header {
	flex-shrink: 0;
	padding: 1.1rem 1.25rem;
	border-bottom: 1px solid #A2C4C9;
}

.inspector-header h3 {
	font-size: 1rem;
	font-weight: 700;
	margin: 0 0 0.5rem 0;
}

.summary-text {
	font-size: 0.8rem;
	color: #FFFFFF;
	line-height: 1.4;
	margin: 0;
}

.box-list {
	flex: 1;
	min-height: 0;
	overflow-y: auto;
	padding: 1rem;
	display: flex;
	flex-direction: column;
	gap: 0.75rem;
}

.box-item {
	flex-shrink: 0;
	background-color: rgba(28, 28, 28, 0.2);
	border: 1px solid #A2C4C9;
	border-radius: 0.5rem;
	padding: 0.75rem;
	transition: border-color 0.2s;
}

.box-item.active {
	border-color: #FFF0DD; /* Bianco soft per la selezione */
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
	color: #FFFFFF;
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
	color: #FFFFFF;
}

.btn-discard {
	background: none;
	border: none;
	color: #D26031; /* Rosso negazione */
	font-size: 0.7rem;
	cursor: pointer;
	text-decoration: underline;
}

/* Scanning Overlay Animation */
.scanning-overlay {
	position: absolute;
	inset: 0;
	background-color: rgba(0, 59, 62, 0.85); /* Versione trasparente del nuovo sfondo scuro */
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
	background: linear-gradient(90deg, transparent, #FFF0DD, #A2C4C9, transparent);
	box-shadow: 0 0 15px #FFF0DD;
	animation: scanMove 2s infinite ease-in-out;
}

@keyframes scanMove {
	0% { top: 0%; }
	50% { top: 100%; }
	100% { top: 0%; }
}

.scanner-card {
	background-color: #003B3E; /* Sfondo principale scuro */
	padding: 1.5rem 2rem;
	border-radius: 0.75rem;
	text-align: center;
	border: 1px solid #A2C4C9;
}

.spinner {
	width: 32px;
	height: 32px;
	border: 3px solid #A2C4C9;
	border-top-color: #FFF0DD;
	border-radius: 50%;
	animation: spin 0.8s linear infinite;
	margin: 0 auto 0.75rem auto;
}

@keyframes spin {
	to { transform: rotate(360deg); }
}
</style>