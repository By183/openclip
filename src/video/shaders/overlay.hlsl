cbuffer OverlayConstants : register(b0)
{
    float4 rect;        // xy = top-left NDC, zw = size NDC
    float4 center_rad;  // xy = center px, z = outer radius px, w = inner radius px
    float4 color;       // rgb = color (0..1), a = opacity (0..1)
    float4 target_size; // xy = target width/height px, zw = unused
};

Texture2D sprite_tex : register(t0);
SamplerState samp    : register(s0);

struct VSOut
{
    float4 pos    : SV_POSITION;
    float2 uv     : TEXCOORD0;
    float2 px_pos : TEXCOORD1;
};

VSOut vs_overlay(uint vid : SV_VertexID)
{
    float2 corner = float2(vid & 1, (vid >> 1) & 1);
    VSOut o;
    float2 ndc = rect.xy + corner * rect.zw;
    o.pos = float4(ndc, 0.0f, 1.0f);
    o.uv = corner;
    o.px_pos = float2((ndc.x * 0.5f + 0.5f) * target_size.x, (0.5f - ndc.y * 0.5f) * target_size.y);
    return o;
}

float4 ps_sprite(VSOut i) : SV_Target
{
    float4 c = sprite_tex.Sample(samp, i.uv);
    c.a *= color.a;
    return c;
}

float4 ps_shape(VSOut i) : SV_Target
{
    float d = length(i.px_pos - center_rad.xy);
    float a_outer = saturate(center_rad.z - d + 0.5f);
    float a_inner = (center_rad.w > 0.0f) ? saturate(d - center_rad.w + 0.5f) : 1.0f;
    float a = a_outer * a_inner * color.a;
    return float4(color.rgb, a);
}
