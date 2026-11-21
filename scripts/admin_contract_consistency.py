"""Check the producer's OpenAPI component projection and all local references."""

def check_contract(api, schema):
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
