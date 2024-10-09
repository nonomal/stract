import { api, type ScoredHost } from '$lib/api';
import { match } from 'ts-pattern';
import type { PageLoad } from './$types';
import { LIMIT_OPTIONS } from './conf';
import { InvalidHostError, UnknownHostError } from '.';

export const load: PageLoad = async (req) => {
  const host = req.url.searchParams.get('site');
  let chosenHosts: string[] = req.url.searchParams.get('chosenHosts')?.split(',') || [];
  let errorMessage: string | undefined = undefined;

  let limit = LIMIT_OPTIONS[0];

  if (req.url.searchParams.get('limit')) {
    const userLimit = parseInt(req.url.searchParams.get('limit')!);
    limit = LIMIT_OPTIONS.includes(userLimit) ? userLimit : limit;
  }

  if (host && host.length > 0) {
    try {
      const res = await api.webgraphHostKnows({ host }).data;

      match(res)
        .with({ _type: 'unknown' }, () => {
          errorMessage = UnknownHostError;
        })
        .with({ _type: 'known' }, async ({ host }) => {
          if (host.length > 0 && !chosenHosts.includes(host)) chosenHosts = [...chosenHosts, host];
        })
        .exhaustive();
    } catch (_) {
      errorMessage = InvalidHostError;
    }
  }

  let similarHosts: ScoredHost[] = [];

  chosenHosts = chosenHosts.filter((host) => host.length > 0);

  if (chosenHosts.length > 0) {
    similarHosts = await api.webgraphHostSimilar({ hosts: chosenHosts, topN: limit }).data;
  }

  return {
    chosenHosts,
    errorMessage,
    similarHosts,
    limit,
    globals: {
      header: {
        divider: true,
      },
    },
  };
};
