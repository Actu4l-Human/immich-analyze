"""Fix INC promotion in the pinned vLLM-Omni image and check quantization behavior.

The upstream wrapper shallow-copies an INCConfig, leaving its parser bound to the
old object. Name remapping updates the new object, but expert selection reads the
old names and allocates unquantized weights. Rebind only that parser; do not alter
checkpoint metadata, quantization precision, or audio/vision modules.
"""
from importlib.util import find_spec
from pathlib import Path

spec = find_spec("vllm_omni")
if spec is None or spec.origin is None:
    raise RuntimeError("The pinned vLLM-Omni package is missing")
target = Path(spec.origin).parent / "quantization" / "inc_config.py"
source = target.read_text(encoding="utf-8")
before = "        omni.__dict__.update(inc.__dict__)\n        return omni"
after = (
    "        omni.__dict__.update(inc.__dict__)\n"
    "        omni.config_parser = type(inc.config_parser)(omni)\n"
    "        return omni"
)
if source.count(before) != 1:
    raise RuntimeError("INC promotion source changed; review this version-pinned fix")
patched = source.replace(before, after, 1)
compile(patched, str(target), "exec")
target.write_text(patched, encoding="utf-8")

# This is a real parser/mapper regression check, with no GPU or model weights.
from torch.nn import Module
from vllm.model_executor.layers.quantization.inc import INCConfig
from vllm.model_executor.models.utils import WeightsMapper
from vllm_omni.quantization.inc_config import OmniINCConfig

config = INCConfig.from_config({
    "bits": 4,
    "group_size": 128,
    "sym": True,
    "packing_format": "auto_round:auto_gptq",
    "block_name_to_quantize": "thinker.model.layers",
    "extra_config": {r".*thinker\.model\.layers\.\d+\.mlp\.gate.*": {"bits": 16}},
})
config.packed_modules_mapping = {}
upgraded = OmniINCConfig.from_inc_config(config)
upgraded.apply_vllm_mapper(WeightsMapper(orig_to_new_prefix={
    "thinker.lm_head": "thinker.language_model.lm_head",
    "thinker.model": "thinker.language_model.model",
    "talker.model": "talker.language_model.model",
}))
layer = Module()
expert_bits = upgraded.get_layer_config(layer, "thinker.language_model.model.layers.0.mlp.experts")[0]
gate_bits = upgraded.get_layer_config(layer, "thinker.language_model.model.layers.0.mlp.gate")[0]
if expert_bits != 4 or gate_bits != 16:
    raise RuntimeError(f"Incorrect quantization mapping: experts={expert_bits}, gates={gate_bits}")
print("INC promotion verified: 4-bit experts, 16-bit gates")
