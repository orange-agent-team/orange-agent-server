"""Check the producer's OpenAPI component projection and all local references."""

def model_manifest_components(canonical):
    """Project the canonical manifest into the self-contained Admin schema namespace."""
    names={key:"Registry"+''.join(word.title() for word in key.split('_'))+"V1" for key in canonical["$defs"]}
    def remap(value):
        if isinstance(value,list):return [remap(v) for v in value]
        if isinstance(value,dict):
            return {k:("#/components/schemas/"+names[v.removeprefix("#/$defs/")] if k=="$ref" else remap(v)) for k,v in value.items()}
        return value
    components={names[key]:remap(value) for key,value in canonical["$defs"].items()}
    components["RegistryManifestV1"]=remap({k:v for k,v in canonical.items() if k not in ("$defs","$schema")})
    return components


def check_contract(api, schema):
    if "RegistryManifestV1" in api["components"]["schemas"]:
        from pathlib import Path
        import json
        canonical=json.loads((Path(__file__).resolve().parents[1]/"contracts/json-schema/model-version.v1.json").read_bytes())
        for name,component in model_manifest_components(canonical).items():
            if api["components"]["schemas"].get(name)!=component:
                raise ValueError("Admin projection differs from canonical model manifest: "+name)

    def normalize(value):
        if isinstance(value, list):
            return [normalize(item) for item in value]
        if isinstance(value, dict):
            result = {k: normalize(v) for k, v in value.items()}
            ref = result.get('$ref')
            if isinstance(ref, str) and ref.startswith('#/components/schemas/'):
                result['$ref'] = '#/$defs/' + ref[len('#/components/schemas/'):]
            return result
        return value

    if normalize(api['components']['schemas']) != schema['$defs']:
        raise ValueError('Admin OpenAPI and JSON Schema differ')

    def check_refs(value):
        if isinstance(value, list):
            for item in value:
                check_refs(item)
        elif isinstance(value, dict):
            if '$ref' in value:
                ref = value['$ref']
                if not ref.startswith('#/'):
                    raise ValueError('Unexpected external contract reference')
                target = api
                for part in ref[2:].split('/'):
                    target = target[part.replace('~1', '/').replace('~0', '~')]
            for item in value.values():
                check_refs(item)

    check_refs(api)
