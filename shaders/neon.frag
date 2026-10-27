#version 450

// The scene is computed in absolute light: linear BT.2020 RGB in nits
// (cd/m²). Only the last step depends on the swapchain: PQ for HDR10, scRGB's
// linear BT.709 with 1.0 = 80 nits, or SDR's sRGB with 203 nits as white.

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

layout(push_constant) uniform Params {
	float time;
	uint mode;
	float peak;
	uint load;
	vec2 resolution;
} p;

const uint MODE_SDR = 0u;
const uint MODE_HDR10 = 1u;
const uint MODE_SCRGB = 2u;

// GLSL matrices are column-major: each vec3 below is a column.
const mat3 BT709_TO_BT2020 = mat3(
	vec3(0.6274, 0.0691, 0.0164),
	vec3(0.3293, 0.9195, 0.0880),
	vec3(0.0433, 0.0114, 0.8956));
const mat3 BT2020_TO_BT709 = mat3(
	vec3(1.6605, -0.1246, -0.0182),
	vec3(-0.5876, 1.1329, -0.1006),
	vec3(-0.0728, -0.0083, 1.1187));

const float SDR_WHITE = 203.0;
const float SCRGB_UNIT = 80.0;

// SMPTE ST 2084 (PQ): nits to signal and back.
const float PQ_M1 = 0.1593017578125;
const float PQ_M2 = 78.84375;
const float PQ_C1 = 0.8359375;
const float PQ_C2 = 18.8515625;
const float PQ_C3 = 18.6875;

vec3 pq_encode(vec3 nits) {
	vec3 y = pow(clamp(nits / 10000.0, 0.0, 1.0), vec3(PQ_M1));
	return pow((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), vec3(PQ_M2));
}

float pq_decode(float signal) {
	float e = pow(clamp(signal, 0.0, 1.0), 1.0 / PQ_M2);
	return 10000.0 * pow(max(e - PQ_C1, 0.0) / (PQ_C2 - PQ_C3 * e), 1.0 / PQ_M1);
}

vec3 srgb_encode(vec3 v) {
	return mix(v * 12.92, 1.055 * pow(v, vec3(1.0 / 2.4)) - 0.055, step(0.0031308, v));
}

float hash(vec2 v) {
	return fract(sin(dot(v, vec2(12.9898, 78.233))) * 43758.5453);
}

// A tube's light at distance `d` from its centre line: a hot core fading into
// three falloffs of glow. `color` has its largest channel at 1, so the core's
// brightest channel is `peak` nits.
vec3 tube(float d, vec3 color, float on) {
	float w = 0.0045;
	float aa = fwidth(d) + 1e-5;
	float core = 1.0 - smoothstep(w - aa, w + aa, d);
	float glow = exp(-d * 70.0) * 0.08 + exp(-d * 16.0) * 0.012 + exp(-d * 4.0) * 0.002;
	return color * p.peak * on * mix(glow, 1.0, core);
}

float sd_segment(vec2 q, vec2 a, vec2 b) {
	vec2 pa = q - a, ba = b - a;
	return length(pa - ba * clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0));
}

float sd_round_box(vec2 q, vec2 half_size, float r) {
	vec2 d = abs(q) - half_size + r;
	return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

vec3 scene(vec2 q, float t) {
	// The wall: dark bricks, a fraction of a nit.
	vec2 b = q * vec2(9.0, 18.0);
	b.x += 0.5 * mod(floor(b.y), 2.0);
	vec2 cell = floor(b), f = fract(b);
	float mortar = step(0.06, f.x) * step(0.1, f.y);
	vec3 light = vec3(mix(0.08, 0.35 + 0.15 * hash(cell), mortar));

	// --load: busywork; the 1e-6 keeps it from being optimised out.
	float acc = 0.0;
	for (uint i = 0u; i < p.load; i++) {
		acc += sin(dot(q, vec2(float(i) + 1.0, 3.0)) + acc);
	}
	light += 1e-6 * acc;

	vec3 magenta = vec3(1.0, 0.0, 0.55);
	vec3 cyan = vec3(0.0, 0.75, 1.0);
	vec3 green = vec3(0.05, 1.0, 0.1);
	vec3 amber = vec3(1.0, 0.45, 0.0);

	// A ring that breathes.
	light += tube(abs(length(q - vec2(-0.55, -0.02)) - (0.17 + 0.012 * sin(t * 2.0))), magenta, 1.0);

	// A wave running across.
	float x = clamp(q.x, -0.85, 0.85);
	float k = 9.0, a = 0.045;
	float y = 0.2 + a * sin(x * k + t * 1.6);
	float slope = a * k * cos(x * k + t * 1.6);
	float d_wave = abs(q.y - y) / sqrt(1.0 + slope * slope) + abs(q.x - x);
	light += tube(d_wave, cyan, 1.0);

	// A turning frame.
	float c = cos(t * 0.35), s = sin(t * 0.35);
	vec2 r = mat2(c, -s, s, c) * (q - vec2(0.55, -0.06));
	light += tube(abs(sd_round_box(r, vec2(0.17, 0.11), 0.04)), green, 1.0);

	// A triangle with a failing transformer.
	float flicker = hash(vec2(floor(t * 11.0), 7.0)) > 0.1 ? 1.0 : 0.12;
	vec2 v0 = vec2(0.0, -0.34), v1 = vec2(-0.13, -0.1), v2 = vec2(0.13, -0.1);
	float d_tri = min(min(sd_segment(q, v0, v1), sd_segment(q, v1, v2)), sd_segment(q, v2, v0));
	light += tube(d_tri, amber, flicker);

	return light;
}

// The reference strip, three rows of fixed patches (see README):
//   1. neutral steps: 0, 1, 10, 100, 203, 400, 1000, 4000 nits
//   2. BT.709 red, BT.2020 red, then green, then blue; each BT.709 one is
//      at its level in 100-nit white, its BT.2020 partner at the same luminance
//   3. a neutral ramp from 0 to 1000 nits, linear in PQ
vec3 strip(vec2 uv) {
	float v = (uv.y - 0.85) / 0.15 * 3.0;
	int row = min(int(v), 2);
	if (fract(v) < 0.08) {
		return vec3(0.0);
	}
	if (row == 0) {
		float steps[8] = float[](0.0, 1.0, 10.0, 100.0, 203.0, 400.0, 1000.0, 4000.0);
		float n = uv.x * 8.0;
		return fract(n) < 0.02 ? vec3(0.0) : vec3(steps[min(int(n), 7)]);
	}
	if (row == 1) {
		float n = uv.x * 6.0;
		if (fract(n) < 0.015) {
			return vec3(0.0);
		}
		int i = min(int(n), 5);
		vec3 primary = vec3(0.0);
		primary[i / 2] = 1.0;
		vec3 bt709 = 100.0 * BT709_TO_BT2020 * primary;
		vec3 luma = vec3(0.2627, 0.6780, 0.0593);
		return i % 2 == 0 ? bt709 : primary * dot(bt709, luma) / dot(primary, luma);
	}
	return vec3(pq_decode(uv.x * pq_encode(vec3(1000.0)).x));
}

void main() {
	// The scene spans about 0.9 either side of the centre; narrower windows
	// zoom out to fit it. It is computed for every pixel, strip or not, so its
	// derivatives (fwidth) stay defined at the edge.
	float aspect = p.resolution.x / p.resolution.y;
	vec2 q = (uv - vec2(0.5, 0.42)) * vec2(aspect, 1.0) * max(1.0, 1.8 / aspect);
	vec3 lit = scene(q, p.time);
	vec3 nits = uv.y > 0.85 ? strip(uv) : lit;

	vec3 rgb;
	if (p.mode == MODE_HDR10) {
		rgb = pq_encode(nits);
	} else if (p.mode == MODE_SCRGB) {
		rgb = BT2020_TO_BT709 * nits / SCRGB_UNIT;
	} else {
		rgb = srgb_encode(clamp(BT2020_TO_BT709 * nits / SDR_WHITE, 0.0, 1.0));
	}
	out_color = vec4(rgb, 1.0);
}
